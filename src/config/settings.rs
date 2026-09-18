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
    #[serde(default)]
    pub tun: TunSection,
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
    /// Web 面板监听地址。None = 默认 127.0.0.1:9090。
    /// 注意：面板的 select 端点无认证，只应绑回环。
    pub web_listen: Option<String>,
    /// **fallback 尝试上限**：按 EWMA 顺序最多试几个候选。
    ///
    /// 3 = 队首 + 两个次优。设 1 表示只用队首、不 fallback；
    /// 设大值在"池子普遍半死"时能救回更多请求，但单请求最坏延迟随之上升
    /// （每个死候选都要烧一个 dial 超时）。
    pub fallback_attempts: usize,
    /// metacubexd 等 clash 风格 dashboard 的静态文件目录（serve 在 /ui/ 下）。
    #[serde(default)]
    pub ui_dir: Option<String>,
}

impl Default for DataPlaneSection {
    fn default() -> Self {
        Self {
            listen: crate::config::DEFAULT_LISTEN.to_string(),
            web_listen: None,
            fallback_attempts: 3,
            ui_dir: None,
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

/// TUN 透明代理配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TunSection {
    /// 是否启用 TUN 透明代理
    pub enabled: bool,
    /// TUN 设备名（如 "utun0"、"tun0"、"wintun"），None = 自动
    pub device: Option<String>,
    /// MTU，None = 自动（通常 1500/9000）
    pub mtu: Option<u16>,
    /// 是否启用 auto-route（全局路由 / fake-ip 路由）
    pub auto_route: bool,
    /// fake-ip CIDR（auto-route=fake-ip 时生效）
    pub fake_ip_cidr: Option<String>,
    /// 排除的 CIDR（不走 TUN，如本地网段、代理服务器 IP）
    pub exclude_cidrs: Vec<String>,
}

impl Default for TunSection {
    fn default() -> Self {
        Self {
            enabled: false,
            device: crate::config::DEFAULT_TUN_DEVICE.map(|s| s.to_string()),
            mtu: crate::config::DEFAULT_TUN_MTU,
            auto_route: crate::config::DEFAULT_TUN_AUTO_ROUTE,
            fake_ip_cidr: Some(crate::config::DEFAULT_TUN_FAKE_IP_CIDR.to_string()),
            exclude_cidrs: crate::config::DEFAULT_TUN_EXCLUDE_CIDRS
                .iter()
                .map(|s| s.to_string())
                .collect(),
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
    #[cfg_attr(not(feature = "meow"), allow(dead_code))] // web 面板仅 meow 模式
    pub web_listen: String,
    pub fallback_attempts: usize,
    pub ui_dir: String,
    pub state: String,
    pub ctl_sock: String,
    pub selector_store: String,
    // TUN 配置
    #[cfg_attr(
        not(all(feature = "meow", feature = "meow-listener")),
        allow(dead_code)
    )]
    pub tun_enabled: bool,
    #[cfg_attr(
        not(all(feature = "meow", feature = "meow-listener")),
        allow(dead_code)
    )]
    pub tun_device: Option<String>,
    #[cfg_attr(
        not(all(feature = "meow", feature = "meow-listener")),
        allow(dead_code)
    )]
    pub tun_mtu: Option<u16>,
    #[cfg_attr(
        not(all(feature = "meow", feature = "meow-listener")),
        allow(dead_code)
    )]
    pub tun_auto_route: bool,
    #[cfg_attr(
        not(all(feature = "meow", feature = "meow-listener")),
        allow(dead_code)
    )]
    pub tun_fake_ip_cidr: Option<String>,
    #[cfg_attr(
        not(all(feature = "meow", feature = "meow-listener")),
        allow(dead_code)
    )]
    pub tun_exclude_cidrs: Vec<String>,
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

/// 运行时可热更的调度/数据面调参（Web 配置面板与 clash_api PATCH 的落点）。
///
/// 从 `Effective` 播种，运行期经 `PATCH /configs` 热改；**不写回 silverq.toml**
/// —— 程序改写用户带注释的 TOML 是破坏性的，持久化仍以手改文件为准，
/// 面板上会提示"重启后回落到文件值"。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeTuning {
    pub capacity: usize,
    pub batch_size: usize,
    pub interval_secs: u64,
    pub timeout_ms: u64,
    pub concurrency: usize,
    pub timeout_penalty: f64,
    pub fallback_attempts: usize,
}

impl RuntimeTuning {
    pub fn from_eff(eff: &Effective) -> Self {
        Self {
            capacity: eff.capacity,
            batch_size: eff.batch_size,
            interval_secs: eff.interval_secs,
            timeout_ms: eff.timeout_ms,
            concurrency: eff.concurrency,
            timeout_penalty: eff.timeout_penalty,
            fallback_attempts: eff.fallback_attempts,
        }
        .validated()
    }

    /// 夹到安全范围。并发 < 批大小时批内排队（buffer_unordered 语义），
    /// 这里不强制并发 ≥ batch，只保证非零与上限，让用户自己权衡。
    pub fn validated(mut self) -> Self {
        self.capacity = self.capacity.clamp(1, 50);
        self.batch_size = self.batch_size.clamp(1, 100);
        self.interval_secs = self.interval_secs.clamp(5, 3600);
        self.timeout_ms = self.timeout_ms.clamp(500, 15_000);
        self.concurrency = self.concurrency.clamp(1, 100);
        self.timeout_penalty = self.timeout_penalty.clamp(100.0, 30_000.0);
        self.fallback_attempts = self.fallback_attempts.clamp(1, 10);
        self
    }
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
            web_listen: std::env::var("SILVERQ_WEB_LISTEN").unwrap_or_else(|_| {
                fc.data_plane
                    .web_listen
                    .clone()
                    .unwrap_or_else(|| "127.0.0.1:9095".into())
            }),
            fallback_attempts: env_parsed_or(
                "SILVERQ_FALLBACK_ATTEMPTS",
                fc.data_plane.fallback_attempts,
            ),
            // TOML 里的 ~ 不经 shell，程序自己展开（paths 同款，别再忘）
            ui_dir: expand_home(env_or(
                "SILVERQ_UI_DIR",
                fc.data_plane
                    .ui_dir
                    .clone()
                    .unwrap_or_else(|| "~/.local/share/silverq/ui".into()),
            )),

            state: env_or("SILVERQ_STATE", fc.paths.state.clone()),
            ctl_sock: env_or("SILVERQ_CTL_SOCK", fc.paths.ctl_sock.clone()),
            selector_store: env_or("SILVERQ_SELECTOR_STORE", fc.paths.selector_store.clone()),
            // TUN 配置
            tun_enabled: env_parsed_or("SILVERQ_TUN_ENABLED", fc.tun.enabled),
            tun_device: std::env::var("SILVERQ_TUN_DEVICE")
                .ok()
                .filter(|s| !s.is_empty())
                .or(fc.tun.device.clone()),
            tun_mtu: std::env::var("SILVERQ_TUN_MTU")
                .ok()
                .and_then(|s| s.parse().ok())
                .or(fc.tun.mtu),
            tun_auto_route: env_parsed_or("SILVERQ_TUN_AUTO_ROUTE", fc.tun.auto_route),
            tun_fake_ip_cidr: std::env::var("SILVERQ_TUN_FAKE_IP_CIDR")
                .ok()
                .filter(|s| !s.is_empty())
                .or(fc.tun.fake_ip_cidr.clone()),
            tun_exclude_cidrs: std::env::var("SILVERQ_TUN_EXCLUDE_CIDRS")
                .ok()
                .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or(fc.tun.exclude_cidrs.clone()),
        }
    }
}
