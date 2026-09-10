//! 控制通道：Unix socket。daemon 侧监听，CLI 侧发一条命令。
//!
//! 协议（单行）：
//!   reload [path]      重读节点表，重建 registry + 调度池（存活节点继承 EWMA）
//!   select <tag>       手动钉住某节点（调度暂停覆盖）
//!   select auto        取消钉住，恢复自动
//!   status             打印当前状态
//! 应答单行：ok ... 或 error ...
#![cfg(unix)]

#[cfg(feature = "meow")]
use crate::meow::Registry;
use crate::node::Node;
use crate::settings::RuntimeTuning;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;

/// 运行时可热更调参的共享句柄。调度循环/inbound/web 三方共用。
pub type SharedTuning = Arc<parking_lot::RwLock<RuntimeTuning>>;

/// 共享控制状态：调度循环、inbound、ctl 三方共用。
pub struct CtlState {
    #[cfg(feature = "meow")]
    pub registry: Registry,
    pub pool: Arc<RwLock<Vec<Node>>>,
    pub selection: Arc<RwLock<Vec<String>>>,
    /// 当前节点表路径（reload 默认重读它）
    pub nodes_path: Mutex<String>,
    /// 手动钉住：true 时调度循环不覆盖 selection。
    /// **必须与调度循环共享同一个 Arc** —— 早先这里是独立的 AtomicBool，
    /// ctl 置位调度循环根本看不到，钉住功能实际是坏的（加锁怎么改都没用，
    /// 因为两边操作的是不同对象）。
    pub pinned: Arc<AtomicBool>,
    /// 钉住目标 tag。
    ///
    /// **selection 的唯一写者是调度循环**：ctl/web 只在这里登记意图，
    /// 由调度循环在自己的临界区里应用。早先 ctl 直接写 selection，
    /// 与批发布形成双写者竞争，实测 4/10 概率被旧 EWMA 结果覆盖 ——
    /// 加锁只能缩小窗口，改成单写者才根治。
    pub pin_target: Arc<Mutex<Option<String>>>,
    /// 运行时调参（PATCH /configs 热改的落点）
    pub tuning: SharedTuning,
    /// 探测 URL（按需单节点测延迟 GET /proxies/{name}/delay 用）
    #[cfg_attr(not(feature = "meow"), allow(dead_code))] // 仅 meow 的 web 模块读
    pub probe_url: String,
    /// metacubexd 静态目录；空 = 不服务 /ui/
    #[cfg_attr(not(feature = "meow"), allow(dead_code))]
    pub ui_dir: String,
    /// tag -> 协议名（clash_api /proxies 的 type 字段；reload 时重建）
    pub protocols: Mutex<std::collections::HashMap<String, String>>,
}

impl CtlState {
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "meow")]
    pub fn new(
        registry: Registry,
        pool: Arc<RwLock<Vec<Node>>>,
        selection: Arc<RwLock<Vec<String>>>,
        nodes_path: String,
        pinned: Arc<AtomicBool>,
        pin_target: Arc<Mutex<Option<String>>>,
        tuning: SharedTuning,
        probe_url: String,
        ui_dir: String,
        protocols: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            registry,
            pool,
            selection,
            nodes_path: Mutex::new(nodes_path),
            pinned,
            pin_target,
            tuning,
            probe_url,
            ui_dir,
            protocols: Mutex::new(protocols),
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(not(feature = "meow"))]
    pub fn new(
        pool: Arc<RwLock<Vec<Node>>>,
        selection: Arc<RwLock<Vec<String>>>,
        nodes_path: String,
        pinned: Arc<AtomicBool>,
        pin_target: Arc<Mutex<Option<String>>>,
        tuning: SharedTuning,
        probe_url: String,
        ui_dir: String,
        protocols: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            pool,
            selection,
            nodes_path: Mutex::new(nodes_path),
            pinned,
            pin_target,
            tuning,
            probe_url,
            ui_dir,
            protocols: Mutex::new(protocols),
        }
    }
}

/// ctl socket 路径。
pub fn ctl_path() -> std::path::PathBuf {
    std::env::var("SILVERQ_CTL_SOCK")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(
                std::env::var("HOME").unwrap_or_default() + "/.local/state/silverq/ctl.sock",
            )
        })
}

pub async fn serve_ctl(state: Arc<CtlState>, sock_path: std::path::PathBuf) -> std::io::Result<()> {
    let path = sock_path;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let _ = std::fs::remove_file(&path); // 清理陈旧 socket
    let listener = UnixListener::bind(&path)?;
    tracing::info!(sock = %path.display(), "silverq ctl socket listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_one(stream, &state).await {
                tracing::debug!("{e}");
            }
        });
    }
}

/// 处理一条命令（单行）。
async fn handle_one(
    mut stream: UnixStream,
    state: &CtlState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncWriteExt;

    let line = read_line(&mut stream).await?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    let cmd = parts.first().copied().unwrap_or("");

    let reply = match cmd {
        "reload" => do_reload(state, parts.get(1).copied()).await,
        "select" => do_select(state, parts.get(1).copied()).await,
        "status" => Ok(do_status(state).await),
        "" => Err("empty command".to_string()),
        other => Err(format!("unknown command: {other}")),
    };

    let text = match reply {
        Ok(msg) => format!("ok {msg}"),
        Err(e) => format!("error {e}"),
    };
    stream.write_all(format!("{text}\n").as_bytes()).await?;
    Ok(())
}

async fn do_reload(state: &CtlState, path_arg: Option<&str>) -> Result<String, String> {
    let path = path_arg
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.nodes_path.lock().clone());

    let specs = crate::nodespec::load_nodes_yaml(&path).map_err(|e| e.to_string())?;

    // 重建 registry（meow）
    #[cfg(feature = "meow")]
    {
        let mut reg = state.registry.write();
        let mut new_reg = std::collections::HashMap::new();
        for spec in &specs {
            match crate::factory::build_proxy(spec) {
                Ok(p) => {
                    new_reg.insert(spec.tag.clone(), p);
                }
                Err(e) => {
                    tracing::warn!(tag = %spec.tag, "{e} — node skipped on reload");
                }
            }
        }
        *reg = new_reg;
    }

    *state.protocols.lock() = specs
        .iter()
        .map(|s| {
            let name = match s.protocol {
                crate::nodespec::Protocol::Vless => "Vless",
                crate::nodespec::Protocol::Trojan => "Trojan",
                crate::nodespec::Protocol::Shadowsocks => "Shadowsocks",
                crate::nodespec::Protocol::Hysteria2 => "Hysteria2",
                crate::nodespec::Protocol::Direct => "Direct",
            };
            (s.tag.clone(), name.to_string())
        })
        .collect();

    // 更新调度池：新增进池、删除出池、存活继承 EWMA
    let pool = {
        let mut guard = state.pool.write().await;
        let old: Vec<Node> = guard.clone();
        let mut new_pool: Vec<Node> = specs
            .iter()
            .map(|s| Node::new(s.tag.clone(), s.server.clone(), s.port))
            .collect();
        for n in new_pool.iter_mut() {
            if let Some(o) = old.iter().find(|o| o.tag == n.tag) {
                n.adopt_score(o);
            }
        }
        *guard = new_pool;
        guard.clone()
    };

    *state.nodes_path.lock() = path.clone();
    if !state.pinned.load(Ordering::Relaxed) {
        let t = state.tuning.read().clone();
        let top = crate::decision::select_top(&pool, t.capacity, t.timeout_penalty);
        *state.selection.write().await = top.clone();
        return Ok(format!("reloaded {path} ({top:?})"));
    }
    Ok(format!("reloaded {path} (pinned, selection unchanged)"))
}
async fn do_select(state: &CtlState, arg: Option<&str>) -> Result<String, String> {
    let tag = arg.ok_or_else(|| "select: missing <tag|auto>".to_string())?;
    if tag == "auto" {
        *state.pin_target.lock() = None;
        state.pinned.store(false, Ordering::Relaxed);
        // 立刻按 EWMA 重算，别等下一轮测速（间隔可能 30s+）。
        // 早先只清标志、不改 selection，导致解钉后 selection 仍是钉住的那
        // 单个节点 —— 钉到死节点再 auto 的话，请求会继续全失败到下一轮。
        let t = state.tuning.read().clone();
        let top =
            crate::decision::select_top(&state.pool.read().await, t.capacity, t.timeout_penalty);
        *state.selection.write().await = top.clone();
        return Ok(format!("auto (unpinned, {top:?})"));
    }
    // 校验 tag 存在（meow 模式查 registry；非 meow 查池）
    #[cfg(feature = "meow")]
    let exists = state.registry.read().contains_key(tag);
    #[cfg(not(feature = "meow"))]
    let exists = state.pool.read().await.iter().any(|n| n.tag == tag);
    if !exists {
        return Err(format!("unknown node: {tag}"));
    }
    // 只登记意图：selection 由调度循环唯一写入（见 pin_target 文档）。
    // 为了让 CLI/面板立刻看到结果，这里也同步写一次 selection ——
    // 调度循环发现 pin_target 与 selection 一致时不会再改动它。
    *state.pin_target.lock() = Some(tag.to_string());
    state.pinned.store(true, Ordering::Relaxed);
    *state.selection.write().await = vec![tag.to_string()];
    Ok(format!("pinned {tag}"))
}

/// web 面板的 select 入口：复用 ctl 的校验与 pinned 语义。
#[cfg(feature = "meow")] // 仅 web 面板调用，web 模块挂 meow feature
pub async fn do_select_public(state: &CtlState, tag: &str) -> String {
    match do_select(state, Some(tag)).await {
        Ok(m) => format!("{{\"ok\":true,\"msg\":\"{m}\"}}"),
        Err(e) => format!("{{\"ok\":false,\"msg\":\"{e}\"}}"),
    }
}

async fn do_status(state: &CtlState) -> String {
    let sel = state.selection.read().await.clone();
    let pool_len = state.pool.read().await.len();
    let pinned = state.pinned.load(Ordering::Relaxed);
    let path = state.nodes_path.lock().clone();
    format!("nodes={pool_len} selection={sel:?} pinned={pinned} nodes_path={path}")
}

/// 读一行（到 '\n' 或长度上限）。
async fn read_line(
    stream: &mut UnixStream,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut b = [0u8; 1];
    loop {
        let n = stream.read(&mut b).await?;
        if n == 0 {
            break;
        }
        buf.push(b[0]);
        if buf.ends_with(b"\n") || buf.len() >= 4096 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// CLI 侧：连接 ctl socket 发一条命令，打印 daemon 应答后返回。
pub async fn client(line: &str) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let path = ctl_path();

    let mut stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            return Err(
                format!("daemon 未运行？ctl socket 不可用 ({}): {e}", path.display()).into(),
            )
        }
    };
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;

    let mut buf: Vec<u8> = Vec::new();
    let mut b = [0u8; 1];
    while stream.read(&mut b).await? > 0 {
        buf.push(b[0]);
        if buf.ends_with(b"\n") || buf.len() >= 8192 {
            break;
        }
    }
    println!("{}", String::from_utf8_lossy(&buf).trim_end());
    Ok(())
}
