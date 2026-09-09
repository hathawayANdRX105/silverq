//! TOML 配置文件：`silverq.toml`。
//!
//! # 优先级
//!
//! 环境变量 > TOML 文件 > 内置默认值。
//! 环境变量保留是为了向后兼容（早期版本只有 env），也方便容器场景；
//! 日常调参建议写 TOML，一处可查。
//!
//! # 加载
//!
//! `serve` 默认读 `./silverq.toml`（`--config <path>` 可指定）。
//! 文件不存在不是错误——全部走默认值，零配置可跑。
use serde::Deserialize;

/// 主配置。TOML 顶层结构。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub scheduler: SchedulerSection,
    #[serde(default)]
    pub data_plane: DataPlaneSection,
    #[serde(default)]
    pub paths: PathsSection,
}

/// 调度器参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SchedulerSection {
    /// 主组容量：EWMA 排名前 N 进 selection
    pub capacity: usize,
    /// 每批测速节点数
    pub batch_size: usize,
    /// 测速轮间隔（秒）
    pub interval_secs: u64,
    /// 单节点测速超时（毫秒）
    pub timeout_ms: u64,
    /// 批内最大并发
    pub concurrency: usize,
    /// 超时扣分幅度（毫秒）
    pub timeout_penalty: f64,
    /// 探测 URL（generate_204 风格）
    #[cfg_attr(not(feature = "meow"), allow(dead_code))] // meow 模式才发探测请求
    pub probe_url: String,
}

impl Default for SchedulerSection {
    fn default() -> Self {
        Self {
            capacity: crate::config::DEFAULT_ACTIVE_CAPACITY,
            batch_size: crate::config::DEFAULT_BATCH_SIZE,
            interval_secs: crate::config::DEFAULT_SLOW_INTERVAL_SECS,
            timeout_ms: crate::config::DEFAULT_TIMEOUT_MS,
            concurrency: crate::config::DEFAULT_CONCURRENCY,
            timeout_penalty: crate::config::DEFAULT_TIMEOUT_PENALTY,
            probe_url: crate::config::DEFAULT_PROBE_URL.to_string(),
        }
    }
}

/// 数据面参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DataPlaneSection {
    /// 监听地址（SOCKS5 / HTTP-CONNECT 混合）
    pub listen: String,
    /// **fallback 尝试上限**：按 EWMA 顺序最多试几个候选。
    ///
    /// 3 = 队首 + 两个次优。设 1 表示只用队首、不 fallback；
    /// 设大值在"池子普遍半死"时能救回更多请求，但单请求最坏延迟随之上升
    /// （每个死候选都要烧一个 dial 超时）。
    pub fallback_attempts: usize,
}

impl Default for DataPlaneSection {
    fn default() -> Self {
        Self {
            listen: crate::config::DEFAULT_LISTEN.to_string(),
            fallback_attempts: 3,
        }
    }
}

/// 路径类配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathsSection {
    /// EWMA 分数存档
    pub state: String,
    /// ctl socket
    pub ctl_sock: String,
    /// 外部 meow kernel 读的 selector store
    pub selector_store: String,
}

/// 展开 TOML 值里的前导 `~/`。shell 不会展开配置文件里的波浪号，
/// 不处理的话 UnixListener::bind 会真的创建一个名叫 `~` 的目录。
fn expand_home(p: String) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => format!("{home}/{rest}"),
            _ => p,
        },
        None => p,
    }
}

impl PathsSection {
    /// 展开所有路径里的 `~/` 后返回。
    pub fn expanded(&self) -> Self {
        Self {
            state: expand_home(self.state.clone()),
            ctl_sock: expand_home(self.ctl_sock.clone()),
            selector_store: expand_home(self.selector_store.clone()),
        }
    }
}

impl Default for PathsSection {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            state: format!("{home}/.local/state/silverq/scores.json"),
            ctl_sock: format!("{home}/.local/state/silverq/ctl.sock"),
            selector_store: format!("{home}/.local/state/silverq-selector.json"),
        }
    }
}

/// 从 TOML 文件加载。文件缺失/为空返回 `FileConfig::default()`；
/// 解析失败返回错误（配置写错了就该大声失败，不能静默用默认值）。
pub fn load(path: &std::path::Path) -> Result<FileConfig, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("配置文件 {} 不存在，使用默认值", path.display());
            return Ok(FileConfig::default());
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(FileConfig::default());
    }
    toml::from_str(&raw).map_err(|e| format!("解析 {}: {e}", path.display()))
}

/// 运行时生效配置：TOML 为底，环境变量覆盖（向后兼容）。
///
/// 每个字段单独覆盖，便于只设一两个变量做临时调整。
pub struct Effective {
    pub capacity: usize,
    pub batch_size: usize,
    pub interval_secs: u64,
    pub timeout_ms: u64,
    pub concurrency: usize,
    pub timeout_penalty: f64,
    #[cfg_attr(not(feature = "meow"), allow(dead_code))] // meow 模式才发探测请求
    pub probe_url: String,
    pub listen: String,
    pub fallback_attempts: usize,
    pub state: String,
    pub ctl_sock: String,
    pub selector_store: String,
}

fn env_or(key: &str, v: String) -> String {
    std::env::var(key).unwrap_or(v)
}

fn env_parsed_or<T: std::str::FromStr>(key: &str, v: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(v)
}

impl Effective {
    pub fn from(fc: &FileConfig) -> Self {
        let fc = FileConfig {
            paths: fc.paths.expanded(),
            ..fc.clone()
        };
        Self {
            capacity: env_parsed_or("SILVERQ_CAPACITY", fc.scheduler.capacity),
            batch_size: env_parsed_or("SILVERQ_BATCH_SIZE", fc.scheduler.batch_size),
            interval_secs: env_parsed_or("SILVERQ_INTERVAL_SECS", fc.scheduler.interval_secs),
            timeout_ms: env_parsed_or("SILVERQ_TIMEOUT_MS", fc.scheduler.timeout_ms),
            concurrency: env_parsed_or("SILVERQ_CONCURRENCY", fc.scheduler.concurrency),
            timeout_penalty: env_parsed_or("SILVERQ_TIMEOUT_PENALTY", fc.scheduler.timeout_penalty),
            probe_url: env_or("SILVERQ_PROBE_URL", fc.scheduler.probe_url.clone()),
            listen: env_or("SILVERQ_LISTEN", fc.data_plane.listen.clone()),
            fallback_attempts: env_parsed_or(
                "SILVERQ_FALLBACK_ATTEMPTS",
                fc.data_plane.fallback_attempts,
            ),
            state: env_or("SILVERQ_STATE", fc.paths.state.clone()),
            ctl_sock: env_or("SILVERQ_CTL_SOCK", fc.paths.ctl_sock.clone()),
            selector_store: env_or("SILVERQ_SELECTOR_STORE", fc.paths.selector_store.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(tag: &str, content: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "silverq-cfg-{tag}-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn missing_file_means_defaults() {
        let cfg = load(std::path::Path::new("/nonexistent/silverq.toml")).unwrap();
        assert_eq!(cfg.data_plane.fallback_attempts, 3);
        assert_eq!(
            cfg.scheduler.capacity,
            crate::config::DEFAULT_ACTIVE_CAPACITY
        );
    }

    #[test]
    fn parses_fallback_attempts() {
        let p = write("fb", "[data_plane]\nfallback_attempts = 5\n");
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.data_plane.fallback_attempts, 5);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // deny_unknown_fields：写错 key 必须大声失败，静默忽略会让人以为生效了
        let p = write("bad", "[scheduler]\ncappacity = 5\n");
        assert!(load(&p).is_err(), "拼写错误的 key 必须报错");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn full_example_parses() {
        let p = write(
            "full",
            r#"
[scheduler]
capacity = 8
interval_secs = 20
probe_url = "http://127.0.0.1:1/"

[data_plane]
listen = "127.0.0.1:19999"
fallback_attempts = 2

[paths]
state = "/tmp/s.json"
"#,
        );
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.scheduler.capacity, 8);
        assert_eq!(cfg.data_plane.listen, "127.0.0.1:19999");
        assert_eq!(cfg.data_plane.fallback_attempts, 2);
        assert_eq!(cfg.paths.state, "/tmp/s.json");
        let _ = std::fs::remove_file(p);
    }
}
