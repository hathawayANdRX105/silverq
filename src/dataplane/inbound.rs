//! 数据面：最小 SOCKS5/HTTP-CONNECT 混合 inbound。
//! 转发目标 = 调度循环维护的"当前 top-N 选择"，经 meow adapter 直连。
//! silverq 自己就是 selector；meow 只负责协议与连接。
#![cfg(feature = "meow")]

use crate::proxy::meow::Registry;
use crate::proxy::route::{Route, RouteOutcome};
use meow_common::Metadata;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 共享选择状态：调度循环写，inbound 每连接读。
pub type SharedSelection = Arc<tokio::sync::RwLock<Vec<String>>>;

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
    /// 从共享调参取当前快照。每连接取一次：配置面板热改对新建连接即时生效。
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

pub async fn run(
    listener_addr: &str,
    registry: Registry,
    selection: SharedSelection,
    tuning: SharedTuning,
    pinned: Arc<AtomicBool>,
    china: Arc<crate::proxy::dns::ChinaSet>,
    routes: Arc<crate::proxy::route::RouteCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(listener_addr).await?;
    tracing::info!(
        addr = listener_addr,
        "silverq inbound listening (SOCKS5/HTTP-CONNECT)"
    );

    loop {
        let (socket, peer) = listener.accept().await?;
        let registry = registry.clone();
        let selection = selection.clone();
        let tuning = tuning.clone();
        let pinned = pinned.clone();
        let china = china.clone();
        let routes = routes.clone();
        tokio::spawn(async move {
            let ctx = ConnCtx {
                registry: &registry,
                selection: &selection,
                tuning,
                pinned,
                china: &china,
                routes: &routes,
            };
            if let Err(e) = handle_one(socket, peer, &ctx).await {
                tracing::debug!(peer = %peer, "{e}");
            }
        });
    }
}

#[derive(PartialEq)]
enum Proto {
    Socks5,
    Http,
}

/// SOCKS5 命令。HTTP-CONNECT 永远是 Connect。
#[derive(PartialEq, Clone, Copy)]
enum Cmd {
    Connect,
    UdpAssociate,
}

pub struct Target {
    pub host: String,
    pub port: u16,
    proto: Proto,
    cmd: Cmd,
}

/// handle_one 的共享上下文（参数打包：7 个以上就被 clippy 拦了）。
struct ConnCtx<'a> {
    registry: &'a Registry,
    selection: &'a SharedSelection,
    tuning: SharedTuning,
    pinned: Arc<AtomicBool>,
    china: &'a crate::proxy::dns::ChinaSet,
    routes: &'a crate::proxy::route::RouteCache,
}

async fn handle_one(
    mut socket: TcpStream,
    peer: SocketAddr,
    ctx: &ConnCtx<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut peek = [0u8; 1];
    socket
        .read_exact(&mut peek)
        .await
        .map_err(|e| format!("head: {e}"))?;

    let target = if peek[0] == 0x05 {
        // SOCKS5 握手协商：greeting(VER already read, NMETHODS, METHODS...) → 选择 no-auth
        socks5_greeting(&mut socket).await?;
        read_socks5_target(&mut socket).await?
    } else if peek[0].is_ascii_uppercase() {
        // HTTP 方法行（CONNECT / GET / ...）；首字节已被读掉，传给解析器补回
        read_http_connect_target(&mut socket, peek[0]).await?
    } else {
        socket
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await
            .ok();
        return Ok(());
    };

    // 调参快照：TCP dial 与 UDP associate 都从这里取，热改对新建连接即时生效
    let dt = DialTuning::snapshot(&ctx.tuning.read());

    // UDP ASSOCIATE：分配中继 socket，回其地址，然后在 TCP 存活期间跑中继循环。
    // 注意：客户端常填 0.0.0.0:0（自己也不知道源地址），所以不能用 host 空判断拦。
    if target.cmd == Cmd::UdpAssociate {
        return handle_udp_associate(socket, ctx.registry, ctx.selection, dt).await;
    }

    if target.host.is_empty() {
        return Ok(()); // 不支持的命令 / 地址类型，协议层已应答
    }

    // 协议应答
    match target.proto {
        // SOCKS5 成功应答必须是完整 10 字节：VER REP RSV ATYP BND.ADDR(4) BND.PORT(2)
        Proto::Socks5 => {
            socket
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await?
        }
        Proto::Http => {
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?
        }
    }

    // 直连判定（跳过候选链）：
    // - 回环/私网目标：语义对齐 TUN 数据面的私网安全网——这类地址只在
    //   silverq 本机/局域网有意义，送进候选链会拨到节点自己的网络。
    // - 国内域名：直连更快；域名形态必须先经真实 DNS 解析——系统解析在
    //   fake-IP 环境返回假地址，按域名直连会被路由回自家隧道。
    //   解析失败回退候选链（表命中但站点异常时仍有代理兜底）。
    enum DirectDial {
        None,
        Host,
        Ip(std::net::IpAddr),
    }
    let mut direct: DirectDial = if ctx.pinned.load(Ordering::Relaxed) {
        // 钉住语义优先于直连判定：钉住 = 所有流量只走该节点、fail 就 fail，
        // 直连旁路会破坏该语义（e2e pinned_* 回归锁定）。
        DirectDial::None
    } else if is_local_target(&target.host) {
        DirectDial::Host
    } else if ctx.china.matches(&target.host) {
        match crate::proxy::dns::resolve_host(&target.host).await {
            Some(ip) => DirectDial::Ip(ip),
            None => {
                tracing::info!(host = %target.host, "国内域名真实 DNS 解析失败，回退代理候选");
                DirectDial::None
            }
        }
    } else {
        DirectDial::None
    };

    // 路由缓存决策（回环/国内/钉住不进缓存：那些是策略或本机语义，非性能选择）。
    // 代理首响应超阈值的域名下次访问触发竞速：直连 + 候选链并行，TCP 先建连者胜。
    let china_hit = ctx.china.matches(&target.host);
    let cache_eligible =
        !ctx.pinned.load(Ordering::Relaxed) && !is_local_target(&target.host) && !china_hit;
    let mut race = false;
    if cache_eligible && matches!(direct, DirectDial::None) {
        match ctx.routes.decide(&target.host) {
            crate::proxy::route::Decision::Direct => {
                match crate::proxy::dns::resolve_host(&target.host).await {
                    Some(ip) => direct = DirectDial::Ip(ip),
                    None => ctx
                        .routes
                        .record(&target.host, Route::Direct, RouteOutcome::Failed),
                }
            }
            crate::proxy::route::Decision::Race => race = true,
            crate::proxy::route::Decision::Proxy => {}
        }
    }

    // 按当前选择顺序逐个尝试（best → 次优），dial 失败自动 fallback
    let order = ctx.selection.read().await.clone();
    let metadata = Metadata {
        network: meow_common::Network::Tcp,
        host: target.host.clone().into(),
        dst_port: target.port,
        ..Default::default()
    };

    // 先在无锁情况下把候选 adapter 克隆出来（截断到 fallback 上限），
    // 避免 guard 跨 await
    let candidates: Vec<_> = {
        let guard = ctx.registry.read();
        pick_candidates(&order, &guard, dt.fallback_attempts)
    };
    // 逐个 dial，**每个都带超时**。
    //
    // 没有这个超时的话，selection[0] 是死节点时 dial_tcp 会挂到内核 TCP 超时
    // （可达数十秒），fallback 根本轮不到下一个候选 —— 实测表现为请求卡满
    // 客户端超时后失败，而后排明明有活节点。超时取测速超时的 2 倍：
    // 测速能过说明这个节点建连一般在测速超时内完成，留 2 倍余量给抖动。
    // fallback 尝试上限来自配置（silverq.toml [data_plane].fallback_attempts，
    // PATCH /configs 可热改）。按 EWMA 顺序最多试 N 个候选：
    // 池子普遍半死时，大值能救回更多请求；但每个死候选都要烧一个 dial 超时，
    // 单请求最坏延迟随之上升。
    let dt = DialTuning::snapshot(&ctx.tuning.read());
    let dial_timeout = dt.dial();
    let mut conn: Option<Box<dyn meow_common::conn::ProxyConn>> = None;
    let mut used_route = Route::Proxy;
    match direct {
        // 强制直连：回环/私网按原样拨（localhost 解析交给系统，回环段不受
        // fake-IP 影响）；国内域名拨已解析的真实 IP。
        DirectDial::Host => {
            let t = std::cmp::max(dial_timeout, dt.first_response());
            conn = dial_direct(
                (target.host.as_str(), target.port),
                t,
                format!("{}/{}", target.host, target.port),
            )
            .await
            .map(|s| Box::new(s) as Box<dyn meow_common::conn::ProxyConn>);
            used_route = Route::Direct;
        }
        DirectDial::Ip(ip) => {
            let t = std::cmp::max(dial_timeout, dt.first_response());
            conn = dial_direct(
                (ip, target.port),
                t,
                format!("{}/{}", target.host, target.port),
            )
            .await
            .map(|s| Box::new(s) as Box<dyn meow_common::conn::ProxyConn>);
            used_route = Route::Direct;
        }
        DirectDial::None => {
            if race {
                // 竞速：直连与候选链并行，TCP 先建连者胜（慢路由域名的专属路径）
                let label = format!("{}/{}", target.host, target.port);
                let host = target.host.clone();
                let port = target.port;
                let direct_f = Box::pin(async move {
                    match crate::proxy::dns::resolve_host(&host).await {
                        Some(ip) => dial_direct((ip, port), dial_timeout, label).await,
                        None => None,
                    }
                });
                let proxy_f = Box::pin(async {
                    for adapter in &candidates {
                        match tokio::time::timeout(dial_timeout, adapter.dial_tcp(&metadata)).await
                        {
                            Ok(Ok(c)) => return Some(c),
                            Ok(Err(e)) => {
                                tracing::info!(tag = adapter.name(), "dial failed: {e}")
                            }
                            Err(_) => {
                                tracing::info!(
                                    tag = adapter.name(),
                                    "dial 超时 {dial_timeout:?}，换下一个候选"
                                )
                            }
                        }
                    }
                    None
                });
                match futures::future::select(direct_f, proxy_f).await {
                    futures::future::Either::Left((res, proxy_f)) => {
                        if let Some(s) = res {
                            conn = Some(Box::new(s) as Box<dyn meow_common::conn::ProxyConn>);
                            used_route = Route::Direct;
                        } else if let Some(c) = proxy_f.await {
                            conn = Some(c);
                        }
                    }
                    futures::future::Either::Right((res, direct_f)) => {
                        if let Some(c) = res {
                            conn = Some(c);
                        } else if let Some(s) = direct_f.await {
                            conn = Some(Box::new(s) as Box<dyn meow_common::conn::ProxyConn>);
                            used_route = Route::Direct;
                        }
                    }
                }
            } else {
                for adapter in &candidates {
                    match tokio::time::timeout(dial_timeout, adapter.dial_tcp(&metadata)).await {
                        Ok(Ok(c)) => {
                            conn = Some(c);
                            break;
                        }
                        Ok(Err(e)) => {
                            // info 级：死候选是运维必须能看到的信号（fallback 计数也靠它）
                            tracing::info!(tag = adapter.name(), "dial failed: {e}");
                        }
                        Err(_) => {
                            tracing::info!(
                                tag = adapter.name(),
                                "dial 超时 {dial_timeout:?}，换下一个候选"
                            );
                        }
                    }
                }
            }
        }
    }
    let conn: Box<dyn meow_common::conn::ProxyConn> = if let Some(c) = conn {
        c
    } else if !matches!(direct, DirectDial::None) || race {
        // 强制直连 / 竞速全败：诚实失败。本机目标送进代理链没有意义；
        // 竞速全败 = 直连与代理都不可达。
        if cache_eligible {
            if race {
                ctx.routes
                    .record(&target.host, Route::Proxy, RouteOutcome::Failed);
            } else {
                ctx.routes
                    .record(&target.host, Route::Direct, RouteOutcome::Failed);
            }
        }
        if target.proto == Proto::Http {
            let _ = socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")
                .await;
        }
        return Ok(());
    } else if ctx.pinned.load(Ordering::Relaxed) {
        // 钉住语义是"只用这个节点"：所有代理候选失败时直连兜底会绕过钉住，
        // 让钉死节点的请求悄悄走直连成功。钉住必须fail 就 fail。
        tracing::info!("已钉住且候选全失败，不做直连兜底（遵守 pin 语义）");
        if target.proto == Proto::Http {
            let _ = socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")
                .await;
        }
        return Ok(());
    } else {
        // 所有代理候选都失败：尝试直连兜底（TCP 直连目标端口）。
        // 直连超时 = max(dial_timeout, first_response)
        let direct_timeout = std::cmp::max(dial_timeout, dt.first_response());
        match dial_direct(
            (target.host.as_str(), target.port),
            direct_timeout,
            format!("{}/{}", target.host, target.port),
        )
        .await
        {
            Some(stream) => Box::new(stream),
            None => {
                if target.proto == Proto::Http {
                    let _ = socket
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")
                        .await;
                }
                return Ok(());
            }
        }
    };
    let _ = peer;

    let fr = relay(socket, conn, dt.first_response()).await;
    if cache_eligible {
        match fr {
            Ok(Some(d)) => ctx
                .routes
                .record(&target.host, used_route, RouteOutcome::Responded(d)),
            Ok(None) => ctx
                .routes
                .record(&target.host, used_route, RouteOutcome::Neutral),
            Err(e) => {
                ctx.routes
                    .record(&target.host, used_route, RouteOutcome::Failed);
                return Err(e);
            }
        }
    } else {
        fr?;
    }
    Ok(())
}

/// 双向中继，带"对端首次响应"超时。
/// Ok(Some(d)) = 有数据，d = 首响应耗时（从转发客户端首包起算，供路由缓存）；
/// Ok(None) = 无数据结束（对端正常关闭/客户端早退）。
/// 客户端侧早退的读写错误也以 Err 返回，会被上游计为路线 Failed——噪声
/// 可接受（一次误标只多触发一次无害竞速）。
async fn relay(
    socket: TcpStream,
    conn: Box<dyn meow_common::conn::ProxyConn>,
    first_response: Duration,
) -> Result<Option<Duration>, Box<dyn std::error::Error + Send + Sync>> {
    let (mut cr, mut cw) = tokio::io::split(socket);
    let (mut pr, mut pw) = tokio::io::split(conn);

    // 1. 先转发客户端的第一批数据（不等就永远收不到响应）
    let mut buf = vec![0u8; 16 * 1024];
    let n = cr.read(&mut buf).await?;
    if n == 0 {
        return Ok(None); // 客户端直接关了
    }
    pw.write_all(&buf[..n]).await?;
    pw.flush().await?;

    // 2. 等对端首次响应；超时 = 黑洞，让调用方感知
    let t0 = std::time::Instant::now();
    let first = match tokio::time::timeout(first_response, pr.read(&mut buf)).await {
        Ok(Ok(0)) => return Ok(None), // 对端正常关闭
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            return Err(format!("对端 {first_response:?} 内无响应（黑洞节点）").into());
        }
    };
    cw.write_all(&buf[..first]).await?;

    // 3. 首次响应已到，进入常规无超时双向拷贝
    let _ = tokio::join!(
        tokio::io::copy(&mut cr, &mut pw),
        tokio::io::copy(&mut pr, &mut cw)
    );
    Ok(Some(t0.elapsed()))
}

/// SOCKS5 握手协商：VER 已被调用方读掉，这里读 NMETHODS + METHODS，
/// 应答 no-auth（0x00）。不支持认证——本地回环端口，鉴权交给绑定地址。
pub async fn socks5_greeting(
    socket: &mut TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut n = [0u8; 1];
    socket.read_exact(&mut n).await?;
    let mut methods = vec![0u8; n[0] as usize];
    if !methods.is_empty() {
        socket.read_exact(&mut methods).await?;
    }
    if !methods.contains(&0x00) {
        socket.write_all(&[0x05, 0xFF]).await.ok(); // 无可接受方法
        return Err("socks5: client requires auth, only no-auth supported".into());
    }
    socket.write_all(&[0x05, 0x00]).await?;
    Ok(())
}

/// 读 SOCKS5 请求（VER CMD RSV ATYP + 地址 + 端口）。
/// 支持 CMD=0x01 CONNECT 与 CMD=0x03 UDP ASSOCIATE；BIND(0x02) 不支持。
pub async fn read_socks5_target(
    socket: &mut TcpStream,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    let mut head = [0u8; 4];
    socket.read_exact(&mut head).await?;
    let cmd = match (head[0], head[1]) {
        (0x05, 0x01) => Cmd::Connect,
        (0x05, 0x03) => Cmd::UdpAssociate,
        _ => {
            // 回 0x07 command not supported（完整 10 字节）
            socket
                .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .ok();
            return Ok(Target {
                host: String::new(),
                port: 0,
                proto: Proto::Socks5,
                cmd: Cmd::Connect,
            });
        }
    };

    let mut host_bytes: Vec<u8> = Vec::new();
    let port = match head[3] {
        0x01 => {
            let mut b = [0u8; 6];
            socket.read_exact(&mut b).await?;
            host_bytes.extend_from_slice(&b[..4]);
            u16::from_be_bytes([b[4], b[5]])
        }
        0x03 => {
            let mut len = [0u8; 1];
            socket.read_exact(&mut len).await?;
            host_bytes.resize(len[0] as usize, 0);
            socket.read_exact(&mut host_bytes).await?;
            let mut p = [0u8; 2];
            socket.read_exact(&mut p).await?;
            u16::from_be_bytes(p)
        }
        0x04 => {
            let mut b = [0u8; 18];
            socket.read_exact(&mut b).await?;
            host_bytes.extend_from_slice(&b[..16]);
            u16::from_be_bytes([b[16], b[17]])
        }
        _ => {
            socket
                .write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .ok();
            return Ok(Target {
                host: String::new(),
                port: 0,
                proto: Proto::Socks5,
                cmd,
            });
        }
    };

    let host = if host_bytes.len() == 4 {
        host_bytes
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(".")
    } else if host_bytes.len() == 16 {
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&host_bytes);
        std::net::Ipv6Addr::from(addr).to_string()
    } else {
        String::from_utf8_lossy(&host_bytes).into_owned()
    };
    Ok(Target {
        host,
        port,
        proto: Proto::Socks5,
        cmd,
    })
}

pub async fn read_http_connect_target(
    socket: &mut TcpStream,
    first_byte: u8,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    // 首字节已被调用方消耗，补回后读完方法行
    let mut line = String::from(first_byte as char);
    let mut b = [0u8; 1];
    loop {
        socket.read_exact(&mut b).await.map_err(|e| e.to_string())?;
        line.push(b[0] as char);
        if b[0] == b'\n' {
            break;
        }
        if line.len() > 1024 {
            return Ok(Target {
                host: String::new(),
                port: 0,
                proto: Proto::Http,
                cmd: Cmd::Connect,
            });
        }
    }

    // 读完剩余头部，直到空行（CRLF CRLF）。
    // 必须精确停在空行后：残留字节会被拷进隧道，污染客户端 TLS ClientHello。
    let mut header_bytes = 0usize;
    loop {
        let mut cur = Vec::with_capacity(64);
        loop {
            socket.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            header_bytes += 1;
            if b[0] == b'\n' {
                break;
            }
            cur.push(b[0]);
            if header_bytes > 16 * 1024 {
                return Err("http connect: headers too large".into());
            }
        }
        // 空行（只剩 \r 或什么都没有）= 头部结束
        if cur.iter().all(|c| *c == b'\r') {
            break;
        }
        if header_bytes > 16 * 1024 {
            return Err("http connect: headers too large".into());
        }
    }

    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("CONNECT") {
        socket
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            .await
            .ok();
        return Ok(Target {
            host: String::new(),
            port: 0,
            proto: Proto::Http,
            cmd: Cmd::Connect,
        });
    }

    let (host, port) = match parts[1].rsplit_once(':') {
        Some((h, p)) => (
            h.trim_end_matches(']').to_string(),
            p.parse::<u16>().map_err(|_| "bad port")?,
        ),
        None => (parts[1].to_string(), 443),
    };

    Ok(Target {
        host,
        port,
        proto: Proto::Http,
        cmd: Cmd::Connect,
    })
}

/// 直连 TCP 目标（带超时）。None = 失败/超时（已记日志）。
async fn dial_direct(
    addr: impl tokio::net::ToSocketAddrs,
    timeout: Duration,
    label: String,
) -> Option<TcpStream> {
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) => {
            tracing::warn!(target = %label, "直连失败: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(target = %label, "直连超时");
            None
        }
    }
}

/// 回环/私网目标判定：IP 字面量（loopback / RFC1918 私网 / 链路本地 /
/// 未指定 / v6 唯一本地）或 `localhost`（含 `*.localhost`，RFC 6761）。
/// 这类地址只在 silverq 本机或本机局域网内可达，送进代理候选链会被拨到
/// 节点自己的 loopback/LAN——轻则死候选烧满超时后才直连兜底（实测本机
/// 面板 12s+，浏览器早超时白屏），重则拿到节点侧的错误内容。
fn is_local_target(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
        }
        Err(_) => false,
    }
}

/// 处理 UDP ASSOCIATE：绑中继 socket → 回其地址 → 跑中继循环直到 TCP 断开。
///
/// RFC 1928 要求 TCP 控制连接是 association 的生命周期锚点：TCP 一断，
/// 服务端必须回收该 association 的所有 UDP 状态。这里用 oneshot 通知中继循环退出。
/// 按 selection 顺序取前 `max_attempts` 个候选 adapter。
///
/// 抽成纯函数以便单测截断逻辑——e2e 层面这个行为被 EWMA 排序的时序淹没，
/// 测不稳（试过三版 e2e 都被"首轮测速改排序"击穿）。
pub fn pick_candidates(
    order: &[String],
    registry: &HashMap<String, Arc<dyn meow_common::adapter::ProxyAdapter>>,
    max_attempts: usize,
) -> Vec<Arc<dyn meow_common::adapter::ProxyAdapter>> {
    order
        .iter()
        .filter_map(|tag| registry.get(tag).cloned())
        .take(max_attempts.max(1))
        .collect()
}

async fn handle_udp_associate(
    mut socket: TcpStream,
    registry: &Registry,
    selection: &SharedSelection,
    tuning: DialTuning,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let control_local = socket.local_addr()?;
    let (relay, relay_addr) = match crate::dataplane::udp::bind_relay(control_local).await {
        Ok(v) => v,
        Err(e) => {
            // 0x01 general failure
            socket
                .write_all(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .ok();
            return Err(format!("udp associate: bind relay failed: {e}").into());
        }
    };

    // 成功应答带上中继地址，客户端后续把数据报发到这里
    let mut reply = vec![0x05, 0x00, 0x00];
    match relay_addr {
        SocketAddr::V4(a) => {
            reply.push(0x01);
            reply.extend_from_slice(&a.ip().octets());
            reply.extend_from_slice(&a.port().to_be_bytes());
        }
        SocketAddr::V6(a) => {
            reply.push(0x04);
            reply.extend_from_slice(&a.ip().octets());
            reply.extend_from_slice(&a.port().to_be_bytes());
        }
    }
    socket.write_all(&reply).await?;
    tracing::debug!(%relay_addr, "udp associate established");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let relay_task = tokio::spawn(crate::dataplane::udp::run_relay(
        relay,
        registry.clone(),
        selection.clone(),
        tuning.fallback_attempts,
        shutdown_rx,
    ));

    // TCP 控制连接读到 EOF = 客户端结束 association
    let mut sink = [0u8; 1];
    loop {
        match socket.read(&mut sink).await {
            Ok(0) => break,    // EOF
            Ok(_) => continue, // 控制连接上不应有数据，忽略
            Err(_) => break,
        }
    }
    let _ = shutdown_tx.send(());
    let _ = relay_task.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_local_target;

    #[test]
    fn local_targets_are_detected() {
        // 回环 / 私网 / 链路本地 / 未指定 / v6 唯一本地
        for host in [
            "127.0.0.1",
            "127.8.8.8",
            "::1",
            "10.0.0.5",
            "172.16.1.1",
            "172.31.255.255",
            "192.168.31.1",
            "169.254.1.1",
            "0.0.0.0",
            "fd00::1",
            "fe80::1",
            "localhost",
            "LocalHost",
            "dash.localhost",
        ] {
            assert!(is_local_target(host), "{host} 应判定为本地目标");
        }
    }

    #[test]
    fn public_targets_are_not_local() {
        // 公网 IP / 正常域名不能误判（否则所有代理流量都被强制直连）
        for host in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",  // 不在 RFC1918
            "192.169.0.1", // 不在 RFC1918
            "www.gstatic.com",
            "2001:4860::1", // 全球单播 v6
        ] {
            assert!(!is_local_target(host), "{host} 不应判定为本地目标");
        }
    }
}
