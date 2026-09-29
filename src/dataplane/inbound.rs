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

/// SOCKS5 命令。HTTP-CONNECT 是 Connect；透明 HTTP 代理（GET/POST 绝对 URI）
/// 是 HttpProxy——后者不回协议应答，上游响应直接流回客户端。
#[derive(PartialEq, Clone, Copy)]
enum Cmd {
    Connect,
    UdpAssociate,
    HttpProxy,
}

pub struct Target {
    pub host: String,
    pub port: u16,
    proto: Proto,
    cmd: Cmd,
    /// 已从客户端读走、隧道建立后需原样回放给上游的字节。
    /// CONNECT / SOCKS5 / 透明代理三者里只有透明代理非空（请求行+全部头部，
    /// 精确到空行），其余协议握手本身不含有效载荷。
    pub replay: Vec<u8>,
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
        // CONNECT：隧道已建立，回 200 后开始双向裸拷贝。
        Proto::Http if target.cmd == Cmd::Connect => {
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?
        }
        // 透明 HTTP 代理：不回协议应答——请求行+头部已随隧道回放给上游，
        // 源站的响应会直接流回客户端；这里写 200 会插进响应流造成污染。
        Proto::Http => {}
    }

    // 直连判定（跳过候选链）：
    // - 回环/私网目标：语义对齐 TUN 数据面的私网安全网——这类地址只在
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
    // 未命中/过期 → 并行竞速（直连 ‖ 首选，上游首字节判胜，#23）；命中 → 按
    // 缓存里曾成功的路线走；条目路线慢 → 同样进竞速。缓存直连命中失败后要
    // 回退代理候选链（proxy_fallback）。
    let china_hit = ctx.china.matches(&target.host);
    let cache_eligible =
        route_cache_eligible(&target.host, ctx.pinned.load(Ordering::Relaxed), china_hit);
    let mut race = false;
    // 缓存驱动的直连（区别于回环/国内的策略直连）：失败后要回退候选链，
    // 且失败要记账，否则一个被墙域名会每次白烧一轮直连超时。
    let mut proxy_fallback = false;
    if cache_eligible && matches!(direct, DirectDial::None) {
        match ctx.routes.decide(&target.host) {
            crate::proxy::route::Decision::Direct => {
                match crate::proxy::dns::resolve_host(&target.host).await {
                    Some(ip) => {
                        direct = DirectDial::Ip(ip);
                        proxy_fallback = true;
                    }
                    None => ctx
                        .routes
                        .record(&target.host, Route::Direct, RouteOutcome::Failed),
                }
            }
            crate::proxy::route::Decision::Race => race = true,
            crate::proxy::route::Decision::Proxy => {}
        }
    }
    // 非幂等纯 HTTP 请求不参与首字节双发（重复投递有副作用）：降级为串行
    // 候选链。CONNECT/SOCKS 的 replay 为空（greeting 后的裸字节双发安全），
    // 幂等方法（GET/HEAD/OPTIONS/TRACE）双发无害。
    if race && !replay_race_safe(&target.replay) {
        race = false;
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
    // 客户端超时后失败，而后排明明有活节点。超时与测速超时等值（早期是
    // 2×，probe 放宽后每个死候选要烧 8s 学费，交互延迟代价太大，已改成等值，
    // 见 DialTuning::dial 的文档）。
    // fallback 尝试上限来自配置（silverq.toml [data_plane].fallback_attempts，
    // PATCH /configs 可热改）。按 EWMA 顺序最多试 N 个候选：
    // 池子普遍半死时，大值能救回更多请求；但每个死候选都要烧一个 dial 超时，
    // 单请求最坏延迟随之上升。
    let dt = DialTuning::snapshot(&ctx.tuning.read());
    let dial_timeout = dt.dial();
    let mut conn: Option<Box<dyn meow_common::conn::ProxyConn>> = None;
    let mut used_route = Route::Proxy;
    // 策略直连：回环/私网按原样拨（localhost 解析交给系统，回环段不受
    // fake-IP 影响）；国内域名与缓存路线拨已解析的真实 IP。
    match direct {
        DirectDial::Host => {
            let t = std::cmp::max(dial_timeout, dt.first_response());
            conn = dial_direct(
                (target.host.as_str(), target.port),
                t,
                format!("{}/{}", target.host, target.port),
            )
            .await
            .map(|s| Box::new(s) as Box<dyn meow_common::conn::ProxyConn>);
            if conn.is_some() {
                used_route = Route::Direct;
            }
        }
        DirectDial::Ip(ip) => {
            // 缓存驱动的直连只做可达性观察：SYN 黑洞用拨号超时即可判定，
            // 预算不再叠首响（首响等待在 relay 侧另有收紧）。否则被墙域名
            // 每次未命中都要烧满 40s 才轮到代理回退，浏览器早超时（#21）。
            // 策略直连（国内域名）没有回退链，保持原预算语义。
            let t = if proxy_fallback {
                dial_timeout
            } else {
                std::cmp::max(dial_timeout, dt.first_response())
            };
            conn = dial_direct(
                (ip, target.port),
                t,
                format!("{}/{}", target.host, target.port),
            )
            .await
            .map(|s| Box::new(s) as Box<dyn meow_common::conn::ProxyConn>);
            if conn.is_some() {
                used_route = Route::Direct;
            } else if proxy_fallback {
                // 缓存直连拨不通：先记一次账，随后代理兜底成功会把缓存改写
                // 成代理路线（连续 2 次直连失败则把条目切到代理，见 route.rs）。
                ctx.routes
                    .record(&target.host, Route::Direct, RouteOutcome::Failed);
            }
        }
        DirectDial::None => {}
    }

    // 代理侧：首字节竞速（#23——直连 ‖ 首选并行判胜、双败并行兜底），或
    // 「未尝试直连 / 直连拨不通 / POST 守卫降级」时的串行候选链兜底。策略
    // 直连（回环/国内）失败不进这里——送进代理链没意义。
    if conn.is_none() {
        if race {
            // 波次1 = 直连 ‖ 首选候选；波次2（双败后）= 第 2、3 名候选并行。
            // 判胜看上游首字节而非谁先建连——TCP 秒连但 TLS 无响应的一侧赢
            // 不了，这正是旧 TCP 竞速的误判盲区。全程自带首字节预算，胜者
            // 路径不再走 relay 的首响等待。
            let label = format!("{}/{}", target.host, target.port);
            let host = target.host.clone();
            let port = target.port;
            let direct_side: SideFut<'_> = Box::pin(async move {
                match crate::proxy::dns::resolve_host(&host).await {
                    Some(ip) => dial_direct((ip, port), dial_timeout, label)
                        .await
                        .map(|s| Box::new(s) as Box<dyn meow_common::conn::ProxyConn>),
                    None => None,
                }
            });
            let mut waves: Vec<Vec<(Route, SideFut<'_>)>> =
                vec![vec![(Route::Direct, direct_side)]];
            if let Some(first) = candidates.first() {
                let md = &metadata;
                waves[0].push((
                    Route::Proxy,
                    Box::pin(async move {
                        match tokio::time::timeout(dial_timeout, first.dial_tcp(md)).await {
                            Ok(Ok(c)) => Some(c),
                            Ok(Err(e)) => {
                                tracing::info!(tag = first.name(), "dial failed: {e}");
                                None
                            }
                            Err(_) => {
                                tracing::info!(
                                    tag = first.name(),
                                    "dial 超时 {dial_timeout:?}，竞速侧弃用"
                                );
                                None
                            }
                        }
                    }),
                ));
            }
            if candidates.len() > 1 {
                let md = &metadata;
                let wave2: Vec<(Route, SideFut<'_>)> = candidates[1..]
                    .iter()
                    .map(|adapter| {
                        let side: SideFut<'_> = Box::pin(async move {
                            match tokio::time::timeout(dial_timeout, adapter.dial_tcp(md)).await {
                                Ok(Ok(c)) => Some(c),
                                Ok(Err(e)) => {
                                    tracing::info!(tag = adapter.name(), "dial failed: {e}");
                                    None
                                }
                                Err(_) => {
                                    tracing::info!(
                                        tag = adapter.name(),
                                        "dial 超时 {dial_timeout:?}，竞速侧弃用"
                                    );
                                    None
                                }
                            }
                        });
                        (Route::Proxy, side)
                    })
                    .collect();
                waves.push(wave2);
            }
            // race 只可能在 cache_eligible 分支里被置位，记录不必再查门禁。
            match race_relay(
                socket,
                &target.replay,
                dial_timeout,
                waves,
                target.proto == Proto::Http,
            )
            .await
            {
                RaceOutcome::Won(route, fr) => {
                    tracing::info!(
                        host = %target.host,
                        route = ?route,
                        fr_ms = fr.as_millis() as u64,
                        "竞速胜出"
                    );
                    ctx.routes
                        .record(&target.host, route, RouteOutcome::Responded(fr));
                    return Ok(());
                }
                RaceOutcome::ClientLeft => {
                    // 未分出胜负：不记账——竞速发起时可能还没有条目，
                    // 凭空造一条会把无证据的路线当结论用。
                    return Ok(());
                }
                RaceOutcome::AllDead => {
                    // 全灭与旧竞速全败同语义：代理侧记一次失败。
                    ctx.routes
                        .record(&target.host, Route::Proxy, RouteOutcome::Failed);
                    return Ok(());
                }
                RaceOutcome::DiedAfterWin(route, e) => {
                    ctx.routes.record(&target.host, route, RouteOutcome::Failed);
                    return Err(e);
                }
            }
        } else if matches!(direct, DirectDial::None) || proxy_fallback {
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
    let conn: Box<dyn meow_common::conn::ProxyConn> = if let Some(c) = conn {
        c
    } else if !matches!(direct, DirectDial::None) {
        // 强制直连（回环/国内）失败、缓存直连+代理兜底全败：诚实失败。本机
        // 目标送进代理链没有意义。缓存直连的失败已在拨号阶段记过账，这里再
        // 记一次会把 direct_fails 一次打满、把条目误切到代理路线。竞速全败
        // 在 race_relay 分支里已记账并直接返回，不会走到这里。
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

    // 缓存直连命中（复用已记录的直连终点，失败后要回退候选链）的首响等待
    // 收紧到拨号超时：被墙域名 TCP 能通但 TLS 被吞时，40s 首响白等会把请求
    // 拖到客户端超时，且这条路径的失败不经过代理回退链（#21）。未命中/慢
    // 条目走竞速（race_relay 自带首字节预算），不经过这里。策略直连与代理
    // 节点保持原预算（代理侧黑洞检测按 4 倍超时设计）。
    let fr_budget = if used_route == Route::Direct && proxy_fallback {
        dial_timeout
    } else {
        dt.first_response()
    };
    let fr = relay(socket, conn, fr_budget, target.replay).await;
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
/// Ok(Some(d)) = 有数据，d = 首响应耗时（从读取上游首响起到收到首字节，供路由缓存）；
/// Ok(None) = 无数据结束（对端正常关闭/客户端早退）。
/// 客户端侧早退的读写错误也以 Err 返回，会被上游计为路线 Failed——噪声
/// 可接受（一次误标只多触发一次无害竞速）。
/// `replay` = 已从客户端读走、需在隧道建立后先回放给上游的字节（透明 HTTP
/// 代理的请求行+头部；CONNECT / SOCKS5 为空）。
async fn relay(
    socket: TcpStream,
    conn: Box<dyn meow_common::conn::ProxyConn>,
    first_response: Duration,
    replay: Vec<u8>,
) -> Result<Option<Duration>, Box<dyn std::error::Error + Send + Sync>> {
    let (mut cr, mut cw) = tokio::io::split(socket);
    let (mut pr, mut pw) = tokio::io::split(conn);

    // 1. 回放已读走的客户端字节（透明代理的请求行+头部），再让两个方向并发跑。
    //    不能先阻塞读客户端首包：HTTP 代理的 GET 没有正文，客户端发完头部就
    //    等响应，serial read→write→read 会死锁；POST 的正文也要等上游读走
    //    头部后才继续发。回放必须放在并发拷贝之前——上游没收到请求行就不会回。
    if !replay.is_empty() {
        pw.write_all(&replay).await?;
        pw.flush().await?;
    }

    // 2. 两个方向并发，任一方向结束即拆隧道（select! 而非 join!：join! 要
    //    等两边都完，黑洞节点的客户端侧会挂到客户端自己超时，比改前更差；
    //    丢掉未完的一侧 ≈ 旧的即时 RST 拆除语义）。
    let mut up_buf = vec![0u8; 16 * 1024];
    let t0 = std::time::Instant::now();
    let up = async {
        // 先等上游首次响应（超时 = 黑洞，让调用方感知），再转常规拷贝
        let first = match tokio::time::timeout(first_response, pr.read(&mut up_buf)).await {
            Ok(Ok(0)) => return Ok(None), // 对端正常关闭
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                return Err(format!("对端 {first_response:?} 内无响应（黑洞节点）").into());
            }
        };
        cw.write_all(&up_buf[..first]).await?;
        let d = t0.elapsed();
        tokio::io::copy(&mut pr, &mut cw).await?;
        Ok(Some(d))
    };
    let down = tokio::io::copy(&mut cr, &mut pw);

    tokio::select! {
        r = up => r,
        // 客户端侧先结束：拆隧道，不区分正常关闭与错误（误标可接受）
        _ = down => Ok(None),
    }
}

/// 首字节双发是否安全：CONNECT/SOCKS5（replay 为空，greeting 后是裸字节，
/// 双发无重复投递问题）或安全方法的透明 HTTP。非幂等方法（POST/PUT/...）
/// 双发会让源站收到两遍副作用请求，只能走串行候选链。
fn replay_race_safe(replay: &[u8]) -> bool {
    replay.is_empty()
        || replay.starts_with(b"GET ")
        || replay.starts_with(b"HEAD ")
        || replay.starts_with(b"OPTIONS ")
        || replay.starts_with(b"TRACE ")
}

/// 竞速侧拨号 future：None = 拨号失败或超时（各侧日志已在闭包内打）。
type SideFut<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Option<Box<dyn meow_common::conn::ProxyConn>>> + Send + 'a,
    >,
>;

/// 首字节竞速的结果（#23）。
#[derive(Debug)]
enum RaceOutcome {
    /// route 赢得首字节；fr = 自竞速发起到首字节的耗时（含拨号，用户实感）
    Won(Route, Duration),
    /// 客户端在分出胜负前离开——没有结论，不记账
    ClientLeft,
    /// 所有波次全败：没有任何一侧回过首字节
    AllDead,
    /// 赢家首字节到手后连接中途死亡（赢了但被掐）——按赢家路线记账
    DiedAfterWin(Route, Box<dyn std::error::Error + Send + Sync>),
}

/// 首字节竞速：把客户端字节投给每个拨号成功的上游侧，谁先回字节谁赢。
///
/// - `replay` = 透明 HTTP 已读走的请求行+头部（CONNECT/SOCKS 为空，协议
///   应答已由上层发出）；随后实时到达的客户端字节同样双发。
/// - `waves` 按波次执行：上一波全灭（拨号失败或首字节超时）才开下一波。
///   调用方组织成 [直连‖首选, 第 2、3 名] 两波——双败并行兜底，不逐个试。
/// - `first_byte_budget` 同时约束各侧拨号与首字节：一侧静默等满预算即弃。
///   用户等待上限是单个竞速窗，而非串行超时叠加。
/// - `write_503` = 目标为 HTTP 语义（与串行全败路径一致补 503；SOCKS 不补）。
async fn race_relay<'a>(
    socket: TcpStream,
    replay: &[u8],
    first_byte_budget: Duration,
    mut waves: Vec<Vec<(Route, SideFut<'a>)>>,
    write_503: bool,
) -> RaceOutcome {
    let (mut cr, mut cw) = tokio::io::split(socket);
    let t0 = std::time::Instant::now();
    // 客户端已读走的首字节：侧建立时全量投递，之后按增量投递（off 记账）
    let mut sent: Vec<u8> = replay.to_vec();
    let mut cbuf = vec![0u8; 16 * 1024];

    struct LiveSide {
        route: Route,
        pr: tokio::io::ReadHalf<Box<dyn meow_common::conn::ProxyConn>>,
        pw: tokio::io::WriteHalf<Box<dyn meow_common::conn::ProxyConn>>,
        /// 已投递给该侧的 sent 字节数
        off: usize,
        /// 首字节截止时刻：从建连起固定，不因事件循环重建 future 而顺延
        deadline: std::time::Instant,
    }

    enum Ev {
        Client(std::io::Result<usize>),
        Side(usize, Option<(usize, Vec<u8>)>),
        Dial(usize, Option<Box<dyn meow_common::conn::ProxyConn>>),
    }

    let mut live: Vec<LiveSide> = Vec::new();
    let mut wave_idx = 0;
    // None = 已 resolve 的失败槽（保留索引到本轮事件处理完，再统一清走）
    let mut pending: Vec<(Route, Option<SideFut<'a>>)> = Vec::new();

    loop {
        if live.is_empty() && pending.is_empty() {
            if wave_idx < waves.len() {
                pending = std::mem::take(&mut waves[wave_idx])
                    .into_iter()
                    .map(|(route, f)| (route, Some(f)))
                    .collect();
                wave_idx += 1;
                continue;
            }
            if write_503 {
                let _ = cw
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")
                    .await;
            }
            return RaceOutcome::AllDead;
        }

        // 事件集：客户端读（先 poll，及时发现客户端离开）｜各未完成拨号
        // （跨迭代保持：shim 只借用槽位，shim 被丢弃不取消拨号本身）｜各
        // 已建连侧的首字节（重新创建的读 future 取消安全，数据不丢）。
        let mut futs: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = Ev> + Send + '_>>> =
            Vec::new();
        futs.push(Box::pin(async {
            let r = cr.read(&mut cbuf).await;
            Ev::Client(r)
        }));
        for (i, slot) in pending.iter_mut().enumerate() {
            if slot.1.is_none() {
                continue;
            }
            futs.push(Box::pin(async move {
                let c = match slot.1.as_mut() {
                    Some(f) => f.await,
                    None => None,
                };
                Ev::Dial(i, c)
            }));
        }
        for (j, side) in live.iter_mut().enumerate() {
            let remaining = side
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            futs.push(Box::pin(async move {
                let mut b = vec![0u8; 16 * 1024];
                let got = if remaining.is_zero() {
                    None
                } else {
                    match tokio::time::timeout(remaining, side.pr.read(&mut b)).await {
                        Ok(Ok(n)) if n > 0 => Some((n, b)),
                        _ => None,
                    }
                };
                Ev::Side(j, got)
            }));
        }

        let (ev, _, rest) = futures::future::select_all(futs).await;
        drop(rest); // 释放对 cr/cbuf/pending/live 的借用，事件数据是自持的

        match ev {
            Ev::Client(Ok(0)) | Ev::Client(Err(_)) => return RaceOutcome::ClientLeft,
            Ev::Client(Ok(n)) => {
                sent.extend_from_slice(&cbuf[..n]);
                for side in &mut live {
                    if side.off < sent.len() && side.pw.write_all(&sent[side.off..]).await.is_ok() {
                        side.off = sent.len();
                    }
                }
            }
            Ev::Dial(i, conn) => {
                let route = pending[i].0;
                pending[i].1 = None;
                if let Some(c) = conn {
                    let (pr, pw) = tokio::io::split(c);
                    let mut side = LiveSide {
                        route,
                        pr,
                        pw,
                        off: 0,
                        deadline: std::time::Instant::now() + first_byte_budget,
                    };
                    if !sent.is_empty() && side.pw.write_all(&sent).await.is_ok() {
                        side.off = sent.len();
                    }
                    live.push(side);
                }
            }
            Ev::Side(j, Some((n, b))) => {
                // 首字节到手：判胜成立，回程首块交回客户端，胜者接管双向流。
                // 耗时在这里定格——首字节才是判胜信号，不等双向拷贝收尾。
                let fr = t0.elapsed();
                let mut win = live.remove(j);
                if win.off < sent.len() {
                    let _ = win.pw.write_all(&sent[win.off..]).await;
                }
                if cw.write_all(&b[..n]).await.is_err() {
                    return RaceOutcome::ClientLeft;
                }
                let wroute = win.route;
                // 败者侧（其他 live 槽与未完成拨号）随返回一并 drop 即拆除
                return tokio::select! {
                    r = tokio::io::copy(&mut win.pr, &mut cw) => match r {
                        // 上游正常收尾：请求已答完
                        Ok(_) => RaceOutcome::Won(wroute, fr),
                        Err(e) => RaceOutcome::DiedAfterWin(wroute, e.into()),
                    },
                    // 客户端读完响应主动关闭：首字节已成立
                    _ = tokio::io::copy(&mut cr, &mut win.pw) => RaceOutcome::Won(wroute, fr),
                };
            }
            Ev::Side(j, None) => {
                live.remove(j);
            }
        }
        pending.retain(|p| p.1.is_some());
    }
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
                replay: vec![],
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
                replay: vec![],
            });
        }
    };

    // 16 字节既可能是真 IPv6，也可能是恰好 16 字符的域名（如
    // "www.bilibili.com"）。ATYP 已经标明客户端意图：0x04 = IPv6，
    // 0x03 = 域名。只有 ATYP=0x04 才该按地址解码，否则按域名原样保留——
    // 误转换会让 china 后缀表失配，国内站点被错误地送进代理候选链。
    let host = if head[3] == 0x04 && host_bytes.len() == 16 {
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&host_bytes);
        std::net::Ipv6Addr::from(addr).to_string()
    } else if head[3] == 0x01 && host_bytes.len() == 4 {
        host_bytes
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(".")
    } else {
        String::from_utf8_lossy(&host_bytes).into_owned()
    };
    Ok(Target {
        host,
        port,
        proto: Proto::Socks5,
        cmd,
        replay: vec![],
    })
}

/// 解析 HTTP 代理请求：CONNECT 或透明代理（GET/POST 绝对 URI）。
///
/// 读走的原始字节全部累积进 `replay`：透明代理模式下请求行+头部要原样
/// 回放给上游（上游看到的是一个完整的 HTTP 请求）；CONNECT 模式不需要回放，
/// 但读路径必须统一——头部必须精确读到空行为止，残留字节会被拷进隧道，
/// 污染客户端 TLS ClientHello。
pub async fn read_http_connect_target(
    socket: &mut TcpStream,
    first_byte: u8,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    // 首字节已被调用方消耗，补回后读完方法行
    let mut raw: Vec<u8> = vec![first_byte];
    let mut b = [0u8; 1];
    loop {
        socket.read_exact(&mut b).await.map_err(|e| e.to_string())?;
        raw.push(b[0]);
        if b[0] == b'\n' {
            break;
        }
        if raw.len() > 1024 {
            return Ok(bad_request(socket).await);
        }
    }

    // 请求行：METHOD SP REQUEST-TARGET SP HTTP-VERSION
    let line = String::from_utf8_lossy(&raw).into_owned();
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(bad_request(socket).await);
    }
    let is_connect = parts[0].eq_ignore_ascii_case("CONNECT");

    // 读完剩余头部，直到空行（CRLF CRLF），全部累积进 raw
    let mut header_bytes = 0usize;
    loop {
        let mut cur = Vec::with_capacity(64);
        loop {
            socket.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            header_bytes += 1;
            raw.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
            cur.push(b[0]);
            if header_bytes > 16 * 1024 {
                return Err("http: headers too large".into());
            }
        }
        // 空行（只剩 \r 或什么都没有）= 头部结束
        if cur.iter().all(|c| *c == b'\r') {
            break;
        }
        if header_bytes > 16 * 1024 {
            return Err("http: headers too large".into());
        }
    }

    if is_connect {
        // CONNECT host:port —— 目标是 authority，默认 443
        let (host, port) = match split_host_port(parts[1], 443) {
            Some(v) => v,
            None => return Ok(bad_request(socket).await),
        };
        return Ok(Target {
            host,
            port,
            proto: Proto::Http,
            cmd: Cmd::Connect,
            replay: vec![],
        });
    }

    // 透明代理：REQUEST-TARGET 必须是绝对 URI（`scheme://authority/path`）。
    // 浏览器/curl 配成 HTTP 代理时永远发绝对 URI；相对 URI（`GET /path`）
    // 说明对端以为在跟源站说话，不是代理客户端，直接 400。
    let uri = parts[1];
    let scheme_end = match uri.find("://") {
        Some(i) => i,
        None => return Ok(bad_request(socket).await),
    };
    let https = uri[..scheme_end].eq_ignore_ascii_case("https");
    let after = &uri[scheme_end + 3..];
    let authority = after.find(['/', '?', '#']).map_or(after, |i| &after[..i]);
    // `http://127.0.0.1:8090@evil.com/` 的主机是 evil.com，不是回环——
    // 若按首个 @ 切分，is_local_target 会被骗成直连本地，绕过代理策略。
    let host_part = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let default_port = if https { 443 } else { 80 };
    let (host, port) = match split_host_port(host_part, default_port) {
        Some(v) => v,
        None => return Ok(bad_request(socket).await),
    };
    if host.is_empty() {
        return Ok(bad_request(socket).await);
    }

    Ok(Target {
        host,
        port,
        proto: Proto::Http,
        cmd: Cmd::HttpProxy,
        replay: raw,
    })
}

/// 从 `host[:port]` 或绝对 URI 的 authority 段拆出主机与端口；
/// 无端口时取 `default_port`。支持 `[::1]:443` / `[::1]` 形式的 IPv6 字面量。
/// None = 端口不是合法 u16（或 IPv6 字面量缺右括号）。
fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // IPv6 字面量：[addr] 或 [addr]:port
        let (addr, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None => default_port,
        };
        Some((addr.to_string(), port))
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) => Some((h.to_string(), p.parse::<u16>().ok()?)),
            None => Some((s.to_string(), default_port)),
        }
    }
}

/// 回 `400 Bad Request` 并返回空 Target（调用方凭空 host 终止处理）。
async fn bad_request(socket: &mut TcpStream) -> Target {
    socket
        .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
        .await
        .ok();
    Target {
        host: String::new(),
        port: 0,
        proto: Proto::Http,
        cmd: Cmd::Connect,
        replay: vec![],
    }
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
/// 本地/回环/私网目标判定：这类目标永远直连，不进代理候选链。
/// （pub：集成测试要断言"userinfo 不能骗过回环判定"；TUN 的 DIRECT 兜底
/// 反查真身后也走同一道信任边界，防 DNS 污染把私网地址喂进数据面。）
pub fn is_local_target(host: &str) -> bool {
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

/// 域名级路线决策（直连 / 竞速 / 代理）是否适用于该目标。
///
/// IP 字面量目标没有"域名 → 路线"这一层：DNS 无可解析，路线缓存里的
/// Direct/Race 记录对它是无意义的。更实际的是，客户端发来的公网 IPv6
/// 目标在本机只有私网 v6 时盲拨必然 `Network is unreachable`，白白烧掉
/// 一次直连尝试（竞速场景还会拖慢整条路径）。这类目标一律交给候选链。
fn route_cache_eligible(host: &str, pinned: bool, china_hit: bool) -> bool {
    !pinned && !is_local_target(host) && !china_hit && host.parse::<std::net::IpAddr>().is_err()
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
    use super::{
        is_local_target, race_relay, read_socks5_target, replay_race_safe, route_cache_eligible,
        RaceOutcome, SideFut,
    };
    use crate::proxy::route::Route;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 假客户端：连上 race_relay 侧的 socket，返回 (客户端句柄, 服务端句柄)。
    async fn client_pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (sock, _) = l.accept().await.unwrap();
        (client, sock)
    }

    /// TCP 秒连但永不出数据的一侧必须输掉竞速（#23 盲区回归）：直连侧先
    /// 连通且全程静默，代理侧晚 100ms 才拨通但立刻回数据 → 代理侧首字节判胜。
    /// 旧 TCP 竞速在这组时序下会选静默的直连侧，然后空等到超时。
    #[tokio::test]
    async fn race_winner_is_first_byte_not_first_connect() {
        // 直连侧：accept 即持有，永不出数据
        let silent_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent_l.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = silent_l.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(s);
        });
        // 代理侧：100ms 后才拨通，连上立刻回首字节
        let resp_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resp_addr = resp_l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = resp_l.accept().await.unwrap();
            s.write_all(b"FIRST-BYTE").await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let dial = |addr: std::net::SocketAddr, delay: Duration| async move {
            tokio::time::sleep(delay).await;
            match tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await {
                Ok(Ok(s)) => Some(Box::new(s) as Box<dyn meow_common::conn::ProxyConn>),
                _ => None,
            }
        };
        let waves: Vec<Vec<(Route, SideFut<'static>)>> = vec![vec![
            (Route::Direct, Box::pin(dial(silent_addr, Duration::ZERO))),
            (
                Route::Proxy,
                Box::pin(dial(resp_addr, Duration::from_millis(100))),
            ),
        ]];

        let (mut client, sock) = client_pair().await;
        let handle = tokio::spawn(race_relay(sock, &[], Duration::from_secs(5), waves, false));

        let mut buf = [0u8; 32];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("客户端应在 2s 内拿到首字节")
            .unwrap();
        assert_eq!(&buf[..n], b"FIRST-BYTE", "应收到代理侧首字节");
        drop(client); // 客户端关闭 → 胜者双向拷贝收尾 → race_relay 返回

        match handle.await.unwrap() {
            RaceOutcome::Won(route, fr) => {
                assert_eq!(route, Route::Proxy, "TCP 先建连但静默的一侧不应赢");
                assert!(
                    fr < Duration::from_secs(1),
                    "判胜耗时应停在首字节，实际 {fr:?}"
                );
            }
            other => panic!("应判胜，实际 {other:?}"),
        }
    }

    /// 双败后第二波并行兜底（#23）：波次 1 的直连与首选都拨号即败，第 2 波
    /// 的候选救回请求——而不是串行逐个试到第三轮。
    #[tokio::test]
    async fn race_second_wave_rescues_after_double_fail() {
        // 无监听的端口：loopback connect 立即 ECONNREFUSED
        let dead_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead_l.local_addr().unwrap();
        drop(dead_l);

        let resp_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resp_addr = resp_l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = resp_l.accept().await.unwrap();
            s.write_all(b"RESCUED").await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let dial = |addr: std::net::SocketAddr| async move {
            match tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await {
                Ok(Ok(s)) => Some(Box::new(s) as Box<dyn meow_common::conn::ProxyConn>),
                _ => None,
            }
        };
        let waves: Vec<Vec<(Route, SideFut<'static>)>> = vec![
            vec![
                (Route::Direct, Box::pin(dial(dead_addr))),
                (Route::Proxy, Box::pin(dial(dead_addr))),
            ],
            vec![(Route::Proxy, Box::pin(dial(resp_addr)))],
        ];

        let (mut client, sock) = client_pair().await;
        let handle = tokio::spawn(race_relay(sock, &[], Duration::from_secs(5), waves, false));

        let mut buf = [0u8; 32];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("第二波应救回客户端")
            .unwrap();
        assert_eq!(&buf[..n], b"RESCUED");
        drop(client);

        match handle.await.unwrap() {
            RaceOutcome::Won(Route::Proxy, _) => {}
            other => panic!("第二波应救回，实际 {other:?}"),
        }
    }

    /// POST 守卫（#23）：非幂等方法的透明 HTTP 不允许双发，否则源站收到
    /// 两遍副作用请求；CONNECT/SOCKS（空 replay）与安全方法放行。
    #[test]
    fn only_safe_requests_are_race_eligible() {
        assert!(replay_race_safe(b""));
        assert!(replay_race_safe(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"));
        assert!(replay_race_safe(b"HEAD / HTTP/1.1\r\nHost: a\r\n\r\n"));
        assert!(replay_race_safe(b"OPTIONS / HTTP/1.1\r\nHost: a\r\n\r\n"));
        assert!(!replay_race_safe(
            b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhello"
        ));
        assert!(!replay_race_safe(b"PUT /x HTTP/1.1\r\nHost: a\r\n\r\n"));
        assert!(!replay_race_safe(b"DELETE /x HTTP/1.1\r\nHost: a\r\n\r\n"));
    }

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
            "240e:97d:10:1402::1:42",
            "2001:4860::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_local_target(host), "{host} 不应判定为本地目标");
        }
    }

    #[test]
    fn ip_literals_opt_out_of_route_decisions() {
        // 客户端发来的 IP 字面量（尤其公网 IPv6）在本机缺该地址族连通性时
        // 盲拨必然 Network is unreachable；它们没有"域名→路线"这一层，
        // 路线缓存不该对它们做直连/竞速决策。
        for host in [
            "8.8.8.8",
            "240e:97d:10:1402::1:42",
            "2001:4860::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(
                !route_cache_eligible(host, false, false),
                "{host} 不应进入域名级路线决策"
            );
        }
        // 域名正常参与（未钉住、非国内、非本地）。
        assert!(route_cache_eligible("www.gstatic.com", false, false));
        // 三类既有旁路保持不变。
        assert!(!route_cache_eligible("www.baidu.com", false, true)); // 国内
        assert!(!route_cache_eligible("www.gstatic.com", true, false)); // 钉住
        assert!(!route_cache_eligible("localhost", false, false)); // 本地
    }

    #[tokio::test]
    async fn sixteen_byte_domain_is_not_mangled_to_ipv6() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;
        // "www.bilibili.com" 恰好 16 字节，旧代码按 host_bytes.len()==16
        // 误判为 IPv6，生成 "7777:772e:..." 这样的伪字面量，导致国内后缀表
        // 失配、站点被错误地送进代理候选链。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (mut tx, (mut rx, _)) =
            tokio::join!(async { TcpStream::connect(addr).await.unwrap() }, async {
                listener.accept().await.unwrap()
            },);
        // ATYP=0x03（域名），长度 16
        let domain = b"www.bilibili.com";
        let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
        req.extend_from_slice(domain);
        req.extend_from_slice(&443u16.to_be_bytes());
        tx.write_all(&req).await.unwrap();
        let target = read_socks5_target(&mut rx).await.unwrap();
        assert_eq!(
            target.host, "www.bilibili.com",
            "16 字节域名必须保持域名形态"
        );

        // 对照：真 IPv6（ATYP=0x04）仍正确解码
        let v6 = [
            0x20, 0x01, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ];
        let mut req2 = vec![0x05, 0x01, 0x00, 0x04];
        req2.extend_from_slice(&v6);
        req2.extend_from_slice(&443u16.to_be_bytes());
        tx.write_all(&req2).await.unwrap();
        let t2 = read_socks5_target(&mut rx).await.unwrap();
        assert_eq!(t2.host, "2001:4860::1", "真 IPv6 必须解码为字面量");
    }
}
