//! inbound 运行时状态：拨号调参、共享句柄与 run 主循环。

use crate::proxy::meow::Registry;
use crate::proxy::route::RouteCache;
use crate::scheduler::node::Node;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

use super::serve::{handle_one, ConnCtx};
/// 共享选择状态：调度循环写，inbound 每连接读。
pub type SharedSelection = Arc<tokio::sync::RwLock<Vec<String>>>;

/// （代理 dial 成败与首字节成功）归因回各 tag 的 [`Node::hp`]；目标无首字节只记路线。
pub type SharedPool = Arc<tokio::sync::RwLock<Vec<Node>>>;

/// 数据面的拨号调参。
///
/// `timeout_ms` 必须从 `settings::Effective` 传进来，不能再读
/// `config::timeout_ms()` —— 后者只看 env，会把 silverq.toml 里的值悄悄丢掉，
/// 结果测速用 2500 而数据面按默认 2000 派生超时（实测存在过的失联）。
#[derive(Clone, Copy)]
pub struct DialTuning {
    /// 测速超时（毫秒）。dial 与首响超时都由它派生。
    pub timeout_ms: u64,
    /// 按 EWMA 顺序最多试几个候选
    pub fallback_attempts: usize,
}

impl DialTuning {
    /// 从共享调参取当前快照。每连接取一次：ctl config-reload 热改对新建连接即时生效。
    pub fn snapshot(tuning: &crate::config::settings::RuntimeTuning) -> Self {
        Self {
            timeout_ms: tuning.timeout_ms,
            fallback_attempts: tuning.fallback_attempts,
        }
    }
}

/// 共享调参句柄。
pub type SharedTuning = crate::ctl::SharedTuning;

impl DialTuning {
    /// dial 单个候选的超时：与测速超时等值。
    /// 早先是 ×2（"留余量给抖动"），但 probe 放宽到 4s 后 ×2 = 8s：
    /// 浏览器每个死候选烧 8s 学费，fallback×3 最坏 24s —— 保命余量
    /// 不该以交互延迟为代价，改成等值（TCP+TLS 正常 <1s 内完成）。
    pub fn dial(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// 建连后等对端首次响应的超时：测速超时的 4 倍。
    ///
    /// 为什么需要：黑洞节点（TCP 连得上、握手"成功"、之后不回数据）在 AEAD 类
    /// 协议上 `dial_tcp` 会立刻返回 Ok —— dial 超时管不到，请求会挂到客户端
    /// 超时（实测 25s）。
    pub fn first_response(&self) -> Duration {
        Duration::from_millis(self.timeout_ms * 4)
    }
}

/// inbound 常驻依赖：参数打包，避免 `run` 超过 clippy 参数上限。
#[derive(Clone)]
pub struct InboundRuntime {
    pub selection: SharedSelection,
    pub tuning: SharedTuning,
    pub pinned: Arc<AtomicBool>,
    pub china: Arc<crate::proxy::dns::ChinaSet>,
    pub routes: Arc<RouteCache>,
    pub pool: SharedPool,
}

pub async fn run(
    listener_addr: &str,
    registry: Registry,
    runtime: InboundRuntime,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(listener_addr).await?;
    tracing::info!(
        addr = listener_addr,
        "silverq inbound listening (SOCKS5/HTTP-CONNECT)"
    );

    loop {
        let (socket, peer) = listener.accept().await?;
        let registry = registry.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let ctx = ConnCtx {
                registry: &registry,
                runtime: &runtime,
            };
            if let Err(e) = handle_one(socket, peer, &ctx).await {
                tracing::debug!(peer = %peer, "{e}");
            }
        });
    }
}

#[derive(PartialEq)]
pub(super) enum Proto {
    Socks5,
    Http,
}

/// SOCKS5 命令。HTTP-CONNECT 是 Connect；透明 HTTP 代理（GET/POST 绝对 URI）
/// 是 HttpProxy——后者不回协议应答，上游响应直接流回客户端。
#[derive(PartialEq, Clone, Copy)]
pub(super) enum Cmd {
    Connect,
    UdpAssociate,
    HttpProxy,
}

pub struct Target {
    pub host: String,
    pub port: u16,
    pub(super) proto: Proto,
    pub(super) cmd: Cmd,
    /// 已从客户端读走、隧道建立后需原样回放给上游的字节。
    /// CONNECT / SOCKS5 / 透明代理三者里只有透明代理非空（请求行+全部头部，
    /// 精确到空行），其余协议握手本身不含有效载荷。
    pub replay: Vec<u8>,
}
