//! 数据面：最小 SOCKS5/HTTP-CONNECT 混合 inbound。
//! 转发目标 = 调度循环维护的"当前 top-N 选择"，经 meow adapter 直连。
//! silverq 自己就是 selector；meow 只负责协议与连接。
#![cfg(feature = "meow")]

use crate::proxy::meow::Registry;
use crate::proxy::route::{Route, RouteCache, RouteOutcome};
use crate::scheduler::node::Node;
use meow_common::Metadata;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
    runtime: &'a InboundRuntime,
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

    // 请求起点：首字节耗时从这一刻量起（含各段拨号/竞速），各路线耗时可比
    let req_start = std::time::Instant::now();

    // 调参快照：TCP dial 与 UDP associate 都从这里取，热改对新建连接即时生效
    let dt = DialTuning::snapshot(&ctx.runtime.tuning.read());

    // UDP ASSOCIATE：分配中继 socket，回其地址，然后在 TCP 存活期间跑中继循环。
    // 注意：客户端常填 0.0.0.0:0（自己也不知道源地址），所以不能用 host 空判断拦。
    if target.cmd == Cmd::UdpAssociate {
        return handle_udp_associate(socket, ctx.registry, &ctx.runtime.selection, dt).await;
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
    let direct: DirectDial = if ctx.runtime.pinned.load(Ordering::Relaxed) {
        // 钉住语义优先于直连判定：钉住 = 所有流量只走该节点、fail 就 fail，
        // 直连旁路会破坏该语义（e2e pinned_* 回归锁定）。
        DirectDial::None
    } else if is_local_target(&target.host) {
        DirectDial::Host
    } else if ctx.runtime.china.matches(&target.host) {
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
    let china_hit = ctx.runtime.china.matches(&target.host);
    let cache_eligible = route_cache_eligible(
        &target.host,
        ctx.runtime.pinned.load(Ordering::Relaxed),
        china_hit,
    );
    let mut race = false;
    // 缓存驱动的直连（区别于回环/国内的策略直连）：失败后要回退候选链，
    // 且失败要记账，否则一个被墙域名会每次白烧一轮直连超时。
    let mut proxy_fallback = false;
    let mut conn: Option<Box<dyn meow_common::conn::ProxyConn>> = None;
    let mut used_route = Route::Proxy;
    // 产生当前 conn 的代理候选 tag（直连为 None）：实际流量成败的 HP 归因落点
    let mut used_proxy_tag: Option<String> = None;
    if cache_eligible && matches!(direct, DirectDial::None) {
        match ctx.runtime.routes.decide(&target.host) {
            crate::proxy::route::Decision::Direct => {
                // 单一拨号预算同时盖住 DNS + TCP（早先两段各带预算，
                // 被墙域名直连侧最坏要烧 4× 预算才轮到代理兜底）。
                let label = format!("{}/{}", target.host, target.port);
                match single_budget_direct(&target.host, target.port, dt.dial(), &label).await {
                    DirectAttempt::Ok(stream) => {
                        conn = Some(Box::new(stream) as Box<dyn meow_common::conn::ProxyConn>);
                        used_route = Route::Direct;
                        proxy_fallback = true;
                    }
                    DirectAttempt::DnsFail => {
                        // DNS 解析失败（预算内）：记账后串行候选链兜底。
                        ctx.runtime.routes.record(
                            &target.host,
                            Route::Direct,
                            RouteOutcome::Failed,
                        );
                    }
                    DirectAttempt::BudgetExhausted => {
                        // 预算耗尽（DNS 或 dial 段）：记账并按拨号失败语义
                        // 压缩后续候选链的首响预算。
                        ctx.runtime.routes.record(
                            &target.host,
                            Route::Direct,
                            RouteOutcome::Failed,
                        );
                        proxy_fallback = true;
                    }
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

    // 候选按重排后的顺序逐个尝试（best → 次优），dial 失败自动 fallback
    // 顺序来自调度器 EWMA 选择，但刚在实际流量里失败过（HP 已扣、
    // `last_runtime_failure` 在冷却窗内）的节点临时后置：下一个连接就换活路，
    // 不等 30–90s 测速轮次。钉住时选择只有一个元素，重排不改变语义。
    let order = {
        let sel = ctx.runtime.selection.read().await.clone();
        let pool = ctx.runtime.pool.read().await;
        reorder_recent_failures(sel, &pool, RECENT_FAILURE_COOLDOWN)
    };
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
    let dt = DialTuning::snapshot(&ctx.runtime.tuning.read());
    let dial_timeout = dt.dial();
    // 策略直连：回环/私网按原样拨（localhost 解析交给系统，回环段不受
    // fake-IP 影响）；国内域名拨已解析的真实 IP。
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
            // 策略直连（国内域名）没有回退链，拨号预算叠首响：TCP 通了还要等
            // 首字节确认路线可用（缓存直连走上面的单预算就地拨号，不再经过这里）。
            let t = std::cmp::max(dial_timeout, dt.first_response());
            conn = dial_direct(
                (ip, target.port),
                t,
                format!("{}/{}", target.host, target.port),
            )
            .await
            .map(|s| Box::new(s) as Box<dyn meow_common::conn::ProxyConn>);
            if conn.is_some() {
                used_route = Route::Direct;
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
            //
            // HP 归因：代理拨号失败即时扣分；目标无首字节可能是目标本身慢，
            // 只记域名路线失败，不误扣全局节点健康。胜者即时加分，
            // 被胜者取消的负方不奖不罚。
            let label = format!("{}/{}", target.host, target.port);
            let host = target.host.clone();
            let port = target.port;
            let direct_side: SideFut<'_> = Box::pin(async move {
                match single_budget_direct(&host, port, dial_timeout, &label).await {
                    DirectAttempt::Ok(stream) => {
                        Some(Box::new(stream) as Box<dyn meow_common::conn::ProxyConn>)
                    }
                    DirectAttempt::DnsFail => {
                        tracing::info!(host = %host, "竞速直连侧 DNS 解析失败");
                        None
                    }
                    DirectAttempt::BudgetExhausted => {
                        tracing::info!(host = %host, "竞速直连侧预算耗尽（DNS+dial）");
                        None
                    }
                }
            });
            let mut waves: Vec<Vec<SideSpec<'_>>> = vec![vec![SideSpec {
                route: Route::Direct,
                // 直连侧失败不归因任何节点
                tag: None,
                fut: direct_side,
            }]];
            if let Some((first_tag, first)) = candidates.first() {
                let md = &metadata;
                let ft = first_tag.clone();
                waves[0].push(SideSpec {
                    route: Route::Proxy,
                    tag: Some(ft.clone()),
                    fut: Box::pin(async move {
                        match tokio::time::timeout(dial_timeout, first.dial_tcp(md)).await {
                            Ok(Ok(c)) => Some(c),
                            Ok(Err(e)) => {
                                tracing::info!(tag = ft.as_str(), "dial failed: {e}");
                                None
                            }
                            Err(_) => {
                                tracing::info!(
                                    tag = ft.as_str(),
                                    "dial 超时 {dial_timeout:?}，竞速侧弃用"
                                );
                                None
                            }
                        }
                    }),
                });
            }
            if candidates.len() > 1 {
                let md = &metadata;
                let wave2: Vec<SideSpec<'_>> = candidates[1..]
                    .iter()
                    .map(|(tag, adapter)| {
                        let t = tag.clone();
                        let log_tag = t.clone();
                        let side: SideFut<'_> = Box::pin(async move {
                            match tokio::time::timeout(dial_timeout, adapter.dial_tcp(md)).await {
                                Ok(Ok(c)) => Some(c),
                                Ok(Err(e)) => {
                                    tracing::info!(tag = log_tag.as_str(), "dial failed: {e}");
                                    None
                                }
                                Err(_) => {
                                    tracing::info!(
                                        tag = log_tag.as_str(),
                                        "dial 超时 {dial_timeout:?}，竞速侧弃用"
                                    );
                                    None
                                }
                            }
                        });
                        SideSpec {
                            route: Route::Proxy,
                            tag: Some(t),
                            fut: side,
                        }
                    })
                    .collect();
                waves.push(wave2);
            }
            // race 只可能在 cache_eligible 分支里被置位，记录不必再查门禁。
            // 首字节账与 HP 归因都在竞速内完成；此处只认结局。
            match race_relay(
                socket,
                &target.replay,
                dial_timeout,
                waves,
                target.proto == Proto::Http,
                req_start,
                RaceCtx {
                    routes: ctx.runtime.routes.as_ref(),
                    host: &target.host,
                    pool: &ctx.runtime.pool,
                },
            )
            .await
            {
                RaceOutcome::Won => {
                    tracing::info!(host = %target.host, "竞速胜出（首字节已记账）");
                    return Ok(());
                }
                RaceOutcome::ClientLeft => {
                    // 未分出胜负：不记账——竞速发起时可能还没有条目，
                    // 凭空造一条会把无证据的路线当结论用。
                    return Ok(());
                }
                RaceOutcome::AllDead => {
                    // 全灭与旧竞速全败同语义：代理侧失败已在竞速内逐侧
                    // 归因，路线级 Failed 也由竞速内记账。
                    return Ok(());
                }
                RaceOutcome::DiedAfterWin(e) => {
                    // 赢家首字节后连接中途死亡：路线 Failed 已在竞速内记账
                    return Err(e);
                }
            }
        } else if matches!(direct, DirectDial::None) || proxy_fallback {
            for (tag, adapter) in &candidates {
                match tokio::time::timeout(dial_timeout, adapter.dial_tcp(&metadata)).await {
                    Ok(Ok(c)) => {
                        conn = Some(c);
                        used_proxy_tag = Some(tag.clone());
                        break;
                    }
                    Ok(Err(e)) => {
                        // info 级：死候选是运维必须能看到的信号（fallback 计数也靠它）
                        tracing::info!(tag = tag.as_str(), "dial failed: {e}");
                        // 实际流量 dial 失败：扣该节点健康度，冷却排序立即生效
                        attribute_runtime(&ctx.runtime.pool, tag, false).await;
                    }
                    Err(_) => {
                        tracing::info!(
                            tag = tag.as_str(),
                            "dial 超时 {dial_timeout:?}，换下一个候选"
                        );
                        attribute_runtime(&ctx.runtime.pool, tag, false).await;
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
    } else if ctx.runtime.pinned.load(Ordering::Relaxed) {
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
        if cache_eligible {
            // 代理候选全败：旧代理首响不再是可信的滞回基线，
            // 后续直连兜底若收到数据，应成为该域名缓存路线。
            ctx.runtime
                .routes
                .record(&target.host, Route::Proxy, RouteOutcome::Failed);
        }
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
            Some(stream) => {
                used_route = Route::Direct;
                Box::new(stream)
            }
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
    // 首字节账簿（缓存门禁内）：relay 在首字节落地的当下记账，不等收尾；
    // 非缓存目标（策略直连）不记账，行为与旧实现一致。
    let recorder = if cache_eligible {
        Some(RouteRecorder::new(
            ctx.runtime.routes.as_ref(),
            &target.host,
            used_route,
        ))
    } else {
        None
    };
    match relay(
        socket,
        conn,
        fr_budget,
        target.replay,
        req_start,
        recorder,
        used_proxy_tag
            .as_deref()
            .map(|tag| (&ctx.runtime.pool, tag)),
    )
    .await
    {
        Ok(RelayOutcome::Completed) => {} // 首字节到账时已给代理节点加 HP
        Ok(RelayOutcome::Neutral) => {
            // 无数据结束（正常关闭/客户端早退）：中性，不奖不罚
        }
        Err(RelayFail::NoFirstByte(e)) => {
            // 上游无首字节可能是目标慢或拒绝服务；路线缓存已记失败，
            // 不把单个域名的故障算成节点全局 HP 失败。
            return Err(e);
        }
        Err(RelayFail::Stream(e)) => {
            // 首字节之后的流错误：可能是客户端早退，归因不确定，HP 保持中性
            return Err(e);
        }
    }
    Ok(())
}

/// 单方向中继结果。
///
/// `Completed` = 隧道流过至少一个上游字节（首字节账已记）；
/// `Neutral` = 无数据结束（对端正常关闭/客户端早退），不奖不罚。
pub enum RelayOutcome {
    Completed,
    Neutral,
}

/// 中继失败分类：首字节之前的上游无响应属于路线失败，代理 dial 失败才扣节点健康；
/// 首字节之后的流错误可能是客户端早退，健康度保持中性。
pub enum RelayFail {
    /// 黑洞 / 无首字节读错：路线缓存记录失败，但不扣节点全局 HP。
    NoFirstByte(Box<dyn std::error::Error + Send + Sync>),
    /// 首字节已流过之后连接中途死亡。
    Stream(Box<dyn std::error::Error + Send + Sync>),
}

/// 路由缓存首字节账簿：首字节落地的当下记账，不等 relay 收尾。
///
/// `first_byte` 保证正常流关闭不二次记账；`terminal` 保证一次连接至多
/// 一条终局记录（NoFirstByte → Failed；首字节后的晚死 → Failed；
/// 无数据结束 → Neutral）。竞速侧的路线账记在 `race_relay` 内部，不走这里。
/// pub：tests/inbound.rs 用它构造 relay 场景，验证「长连接仍开着时
/// 缓存里已有该路线条目」。
pub struct RouteRecorder<'a> {
    routes: &'a RouteCache,
    host: &'a str,
    route: Route,
    first_byte: AtomicBool,
    terminal: AtomicBool,
}

impl<'a> RouteRecorder<'a> {
    /// 新建账簿：路线 = `route`（调用方按自己的 `used_route` 传入）。
    pub fn new(routes: &'a RouteCache, host: &'a str, route: Route) -> Self {
        Self {
            routes,
            host,
            route,
            first_byte: AtomicBool::new(false),
            terminal: AtomicBool::new(false),
        }
    }

    /// 首字节落地记账（幂等：二次调用不重复记账）。
    fn first_byte(&self, fr: Duration) {
        if !self.first_byte.swap(true, Ordering::Relaxed) {
            self.routes
                .record(self.host, self.route, RouteOutcome::Responded(fr));
        }
    }

    /// 终局记账：至多一次。`no_first_byte_failed` = 首字节前的失败
    /// （黑洞/读错）；首字节已落地后的晚死同样记 Failed。
    fn terminal(&self, no_first_byte_failed: bool) {
        if self.terminal.swap(true, Ordering::Relaxed) {
            return;
        }
        if self.first_byte.load(Ordering::Relaxed) || no_first_byte_failed {
            self.routes
                .record(self.host, self.route, RouteOutcome::Failed);
        } else {
            self.routes
                .record(self.host, self.route, RouteOutcome::Neutral);
        }
    }
}

/// 双向中继，带"对端首次响应"超时。
///
/// 返回 [`RelayOutcome`] / [`RelayFail`]：首字节落地即向 `recorder`
/// 记账（`req_start` 到首字节的耗时，含拨号段，各路线可比），不等收尾；
/// `replay` = 已从客户端读走、需在隧道建立后先回放给上游的字节（透明 HTTP
/// 代理的请求行+头部；CONNECT / SOCKS5 为空）。
/// pub：tests/inbound.rs 验证「首字节落地即记账、长连接仍开着」。
/// `proxy_feedback` 仅在响应成功写回客户端时加 HP，不等长连接关闭。
pub async fn relay(
    socket: TcpStream,
    conn: Box<dyn meow_common::conn::ProxyConn>,
    first_response: Duration,
    replay: Vec<u8>,
    req_start: std::time::Instant,
    recorder: Option<RouteRecorder<'_>>,
    proxy_feedback: Option<(&SharedPool, &str)>,
) -> Result<RelayOutcome, RelayFail> {
    let (mut cr, mut cw) = tokio::io::split(socket);
    let (mut pr, mut pw) = tokio::io::split(conn);

    // 1. 回放已读走的客户端字节（透明代理的请求行+头部），再让两个方向并发跑。
    //    不能先阻塞读客户端首包：HTTP 代理的 GET 没有正文，客户端发完头部就
    //    等响应，serial read→write→read 会死锁；POST 的正文也要等上游读走
    //    头部后才继续发。回放必须放在并发拷贝之前——上游没收到请求行就不会回。
    if !replay.is_empty() {
        if let Err(e) = pw.write_all(&replay).await {
            if let Some(r) = &recorder {
                r.terminal(true);
            }
            return Err(RelayFail::NoFirstByte(e.into()));
        }
        if let Err(e) = pw.flush().await {
            if let Some(r) = &recorder {
                r.terminal(true);
            }
            return Err(RelayFail::NoFirstByte(e.into()));
        }
    }

    // 2. 两个方向并发，任一方向结束即拆隧道（select! 而非 join!：join! 要
    //    等两边都完，黑洞节点的客户端侧会挂到客户端自己超时，比改前更差；
    //    丢掉未完的一侧 ≈ 旧的即时 RST 拆除语义）。
    let mut up_buf = vec![0u8; 16 * 1024];
    let up = async {
        // 先等上游首次响应（超时 = 黑洞，让调用方感知），再转常规拷贝
        let first = match tokio::time::timeout(first_response, pr.read(&mut up_buf)).await {
            Ok(Ok(0)) => {
                // 无数据正常关闭：记一条 Neutral（续 TTL），不奖不罚
                if let Some(r) = &recorder {
                    r.terminal(false);
                }
                return Ok(RelayOutcome::Neutral);
            }
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                if let Some(r) = &recorder {
                    r.terminal(true);
                }
                return Err(RelayFail::NoFirstByte(e.into()));
            }
            Err(_) => {
                if let Some(r) = &recorder {
                    r.terminal(true);
                }
                return Err(RelayFail::NoFirstByte(
                    format!("对端 {first_response:?} 内无响应（黑洞节点）").into(),
                ));
            }
        };
        // 首字节落定的当下记账：不等 copy 收尾、不等长连接关闭
        if let Some(r) = &recorder {
            r.first_byte(req_start.elapsed());
        }
        if let Err(e) = cw.write_all(&up_buf[..first]).await {
            if let Some(r) = &recorder {
                r.terminal(false);
            }
            return Err(RelayFail::Stream(e.into()));
        }
        if let Some((pool, tag)) = proxy_feedback {
            attribute_runtime(pool, tag, true).await;
        }
        match tokio::io::copy(&mut pr, &mut cw).await {
            Ok(_) => Ok(RelayOutcome::Completed),
            Err(e) => {
                if let Some(r) = &recorder {
                    r.terminal(false);
                }
                Err(RelayFail::Stream(e.into()))
            }
        }
    };
    let down = async {
        // 客户端侧先结束：拆隧道，正常关闭与错误不区分（早退不奖不罚）
        let _ = tokio::io::copy(&mut cr, &mut pw).await;
        Ok(RelayOutcome::Neutral)
    };

    tokio::select! {
        r = up => r,
        r = down => r,
    }
}

/// 首字节双发是否安全：CONNECT/SOCKS5（replay 为空，greeting 后是裸字节，
/// 双发无重复投递问题）或安全方法的透明 HTTP。非幂等方法（POST/PUT/...）
/// 双发会让源站收到两遍副作用请求，只能走串行候选链。
pub fn replay_race_safe(replay: &[u8]) -> bool {
    replay.is_empty()
        || replay.starts_with(b"GET ")
        || replay.starts_with(b"HEAD ")
        || replay.starts_with(b"OPTIONS ")
        || replay.starts_with(b"TRACE ")
}

/// 一次竞速的记账目标：路线缓存 + 域名 + 节点池（参数打包：race_relay
/// 拆出它才回到 clippy 参数上限内）。pub：tests/inbound.rs 组装真实
/// loopback 竞速。
pub struct RaceCtx<'a> {
    pub routes: &'a RouteCache,
    pub host: &'a str,
    pub pool: &'a SharedPool,
}
/// 竞速侧拨号 future：None = 拨号失败或超时（各侧日志已在闭包内打）。
pub type SideFut<'a> = std::pin::Pin<
    Box<dyn Future<Output = Option<Box<dyn meow_common::conn::ProxyConn>>> + Send + 'a>,
>;

/// 竞速的一个参与侧：路线 + HP 归因 tag（直连侧为 None）+ 拨号 future。
/// pub：tests/inbound.rs 用真实 loopback 竞速验证「首字节落地即记账」。
pub struct SideSpec<'a> {
    pub route: Route,
    pub tag: Option<String>,
    pub fut: SideFut<'a>,
}

/// 首字节竞速的结果（#23）。路线账与健康度归因都在竞速内部完成，
/// 调用方只认结局。pub：tests/inbound.rs 的早期记账场景按结局断言收尾行为。
#[derive(Debug)]
pub enum RaceOutcome {
    /// 某侧赢得首字节（首字节账已即时记入路由缓存；代理侧健康度已加）
    Won,
    /// 客户端在分出胜负前离开——没有结论，不记账
    ClientLeft,
    /// 所有波次全败：没有任何一侧回过首字节（路线级 Failed 已记账）
    AllDead,
    /// 赢家首字节到手后连接中途死亡（赢了但被掐）——路线 Failed 已记账
    DiedAfterWin(Box<dyn std::error::Error + Send + Sync>),
}

/// 首字节竞速：把客户端字节投给每个拨号成功的上游侧，谁先回字节谁赢。
///
/// - `replay` = 透明 HTTP 已读走的请求行+头部（CONNECT/SOCKS 为空，协议
///   应答已由上层发出）；随后实时到达的客户端字节同样双发。
/// - `waves` 按波次执行：上一波全灭（拨号失败或首字节超时）才开下一波。
///   调用方组织成 [直连‖首选, 第 2、3 名] 两波——双败并行兜底，不逐个试。
/// - `first_byte_budget` 同时约束各侧拨号与首字节：一侧静默等满预算即弃。
///   用户等待上限是单个竞速窗，而非串行超时叠加。
/// - `req_start`：请求起点（含各段拨号/竞速前的准备），首字节耗时 =
///   `req_start` 到首字节的墙钟时间，与 relay 口径一致（含拨号段）。
/// - 路线账：首字节落地即 `Responded(fr)`（不等 copy 收尾）；赢家晚死
///   或全灭各记一条 `Failed`。
/// - HP 归因：代理拨号失败 `note_proxy_failure`；首字节胜者
///   `note_proxy_success`；目标无首字节和被取消的败方不影响全局 HP。
/// - `write_503` = 目标为 HTTP 语义（与串行全败路径一致补 503；SOCKS 不补）。
/// - `ctx`：路线缓存、域名、节点池（记账目标）。
///
/// pub：tests/inbound.rs 用 loopback 竞速验证「首字节落地即记账、连接仍
/// 开着」的早期路线/HP 归因，不等 copy 收尾。
pub async fn race_relay<'a>(
    socket: TcpStream,
    replay: &[u8],
    first_byte_budget: Duration,
    mut waves: Vec<Vec<SideSpec<'a>>>,
    write_503: bool,
    req_start: std::time::Instant,
    ctx: RaceCtx<'_>,
) -> RaceOutcome {
    let (mut cr, mut cw) = tokio::io::split(socket);
    // 客户端已读走的首字节：侧建立时全量投递，之后按增量投递（off 记账）
    let mut sent: Vec<u8> = replay.to_vec();
    let mut cbuf = vec![0u8; 16 * 1024];

    struct LiveSide {
        route: Route,
        tag: Option<String>,
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
    let mut pending: Vec<(Route, Option<String>, Option<SideFut<'a>>)> = Vec::new();

    loop {
        if live.is_empty() && pending.is_empty() {
            if wave_idx < waves.len() {
                pending = std::mem::take(&mut waves[wave_idx])
                    .into_iter()
                    .map(|s| (s.route, s.tag, Some(s.fut)))
                    .collect();
                wave_idx += 1;
                continue;
            }
            // 全灭：代理侧各失败已逐侧归因（HP），路线级再记一条 Failed
            ctx.routes
                .record(ctx.host, Route::Proxy, RouteOutcome::Failed);
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
        let mut futs: Vec<std::pin::Pin<Box<dyn Future<Output = Ev> + Send + '_>>> = Vec::new();
        futs.push(Box::pin(async {
            let r = cr.read(&mut cbuf).await;
            Ev::Client(r)
        }));
        for (i, slot) in pending.iter_mut().enumerate() {
            if slot.2.is_none() {
                continue;
            }
            futs.push(Box::pin(async move {
                let c = match slot.2.as_mut() {
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
                let tag = pending[i].1.clone();
                pending[i].2 = None;
                if let Some(c) = conn {
                    let (pr, pw) = tokio::io::split(c);
                    let mut side = LiveSide {
                        route,
                        tag: tag.clone(),
                        pr,
                        pw,
                        off: 0,
                        deadline: std::time::Instant::now() + first_byte_budget,
                    };
                    if !sent.is_empty() && side.pw.write_all(&sent).await.is_ok() {
                        side.off = sent.len();
                    }
                    live.push(side);
                } else if let Some(t) = &tag {
                    // dial 失败/超时的代理侧 = 实际流量失败：扣健康度
                    attribute_runtime(ctx.pool, t, false).await;
                }
            }
            Ev::Side(j, Some((n, b))) => {
                // 首字节到手：判胜成立，回程首块交回客户端，胜者接管双向流。
                // 路线账在此刻记下（不等 copy 收尾），HP 即时归因给代理侧。
                let fr = req_start.elapsed();
                let mut win = live.remove(j);
                if win.off < sent.len() {
                    let _ = win.pw.write_all(&sent[win.off..]).await;
                }
                if cw.write_all(&b[..n]).await.is_err() {
                    // 首字节落地但客户端写失败：客户端侧早退，负方保持中性
                    return RaceOutcome::ClientLeft;
                }
                let wroute = win.route;
                ctx.routes
                    .record(ctx.host, wroute, RouteOutcome::Responded(fr));
                if let Some(t) = &win.tag {
                    attribute_runtime(ctx.pool, t, true).await;
                }
                // 败者侧（其他 live 槽与未完成拨号）随返回一并 drop 即拆除
                return tokio::select! {
                    r = tokio::io::copy(&mut win.pr, &mut cw) => match r {
                        Ok(_) => RaceOutcome::Won,
                        Err(e) => {
                            ctx.routes.record(ctx.host, wroute, RouteOutcome::Failed);
                            RaceOutcome::DiedAfterWin(e.into())
                        }
                    },
                    _ = tokio::io::copy(&mut cr, &mut win.pw) => RaceOutcome::Won,
                };
            }
            Ev::Side(j, None) => {
                // 已拨通但目标没回首字节：此侧出局，给下波候选机会；
                // 无法单靠这次超时判定节点全局故障，HP 保持中性。
                live.remove(j);
            }
        }
        pending.retain(|p| p.2.is_some());
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

/// 单预算直连的结果：区分「预算内 DNS 失败」（域名没解析出来）与
/// 「预算耗尽」（DNS 或 dial 段超时）——两者回退语义不同：
/// DNS 失败不压缩候选链首响预算（解析问题，代理侧正常探测即可），
/// 拨号失败才压缩（链路本身在死，别再白等）。
enum DirectAttempt {
    /// 直连成功（流已建立）
    Ok(TcpStream),
    /// 预算内 DNS 解析失败
    DnsFail,
    /// DNS 或 dial 段耗尽单一预算
    BudgetExhausted,
}

/// 直连（DNS 解析 + TCP dial）单一预算：`budget` 同时约束两段，
/// 不叠加（早先 DNS 超时 + dial 超时各一份，被墙域名直连侧最坏
/// 4× 预算才轮到代理兜底，浏览器早超时）。
async fn single_budget_direct(
    host: &str,
    port: u16,
    budget: Duration,
    label: &str,
) -> DirectAttempt {
    let fused = async {
        let ip = crate::proxy::dns::resolve_host(host).await;
        let Some(ip) = ip else {
            tracing::info!(target = %label, "直连 DNS 解析失败（预算内）");
            return DirectAttempt::DnsFail;
        };
        match dial_direct((ip, port), budget, label.to_string()).await {
            Some(s) => DirectAttempt::Ok(s),
            None => DirectAttempt::BudgetExhausted,
        }
    };
    match tokio::time::timeout(budget, fused).await {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(target = %label, "直连单预算耗尽（DNS+dial）");
            DirectAttempt::BudgetExhausted
        }
    }
}

/// 数据面近期失败冷却窗：实际流量里代理 dial 失败过的节点，
/// 在冷却窗内（`Node::last_runtime_failure`）被候选重排临时后置——
pub const RECENT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// 把实际流量成败归因回调度节点池的健康度（HP）。
///
/// 只写 HP 与 `last_runtime_failure`，不碰 `consecutive_failures`（那是
/// 调度侧「探测失败」计数，数据面失败不共享）：一次成功的调度探测
/// 会清掉失败计数、让节点复活，但冷却排序仍凭 `last_runtime_failure`
/// 把刚被实际流量证实失败的节点压在后面——两条证据线互不覆盖。
///
/// 成功 = 实际流量首字节成功（+HP，清瞬态标记）；失败 = 代理拨号失败
/// （-HP，打瞬态时刻）。目标无首字节只影响域名路线，客户端早退或
/// 竞速负方等中性结局不改变节点健康度。
pub async fn attribute_runtime(pool: &SharedPool, tag: &str, success: bool) {
    let mut nodes = pool.write().await;
    apply_runtime_outcome(&mut nodes, tag, success);
}

/// 实际流量结果的 HP 记账（纯函数，便于单测）。
///
/// 成功：`note_proxy_success`（+HP，清 `last_runtime_failure`）；
/// 失败：`note_proxy_failure`（-HP，打 `last_runtime_failure` 时刻）。
/// 池里没有该 tag（节点刚被 reload 摘除等）：静默跳过。
pub fn apply_runtime_outcome(nodes: &mut [Node], tag: &str, success: bool) {
    let Some(n) = nodes.iter_mut().find(|n| n.tag == tag) else {
        return;
    };
    if success {
        n.note_proxy_success();
    } else {
        n.note_proxy_failure();
    }
}

/// 候选重排：把冷却窗内失败过的节点后置，其余保持调度器 EWMA 顺序。
///
/// 失败证据来自 `Node::last_runtime_failure`（实际流量代理 dial 失败
/// 打的瞬态时刻，不是目标首字节缺失或调度测速的失败计数）。冷却窗外恢复原序——
pub fn reorder_recent_failures(
    mut order: Vec<String>,
    nodes: &[Node],
    cooldown: Duration,
) -> Vec<String> {
    let now = std::time::Instant::now();
    let mut current = 0;
    for _ in 0..order.len() {
        let failed_recently = nodes
            .iter()
            .find(|n| n.tag == order[current])
            .and_then(|n| n.last_runtime_failure)
            .is_some_and(|t| now.saturating_duration_since(t) < cooldown);
        if failed_recently {
            let tag = order.remove(current);
            order.push(tag);
        } else {
            current += 1;
        }
    }
    order
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
pub fn route_cache_eligible(host: &str, pinned: bool, china_hit: bool) -> bool {
    !pinned && !is_local_target(host) && !china_hit && host.parse::<std::net::IpAddr>().is_err()
}

/// 按 selection 顺序取前 `max_attempts` 个候选 adapter，返回 (tag, adapter) 对。
/// tag 是实际流量成败的 HP 归因落点；缺失 tag（配置漂移/节点被摘）跳过。
///
/// 抽成纯函数以便单测截断逻辑——e2e 层面这个行为被 EWMA 排序的时序淹没，
/// 测不稳（试过三版 e2e 都被"首轮测速改排序"击穿）。
pub fn pick_candidates(
    order: &[String],
    registry: &HashMap<String, Arc<dyn meow_common::adapter::ProxyAdapter>>,
    max_attempts: usize,
) -> Vec<(String, Arc<dyn meow_common::adapter::ProxyAdapter>)> {
    order
        .iter()
        .filter_map(|tag| registry.get(tag).map(|a| (tag.clone(), a.clone())))
        .take(max_attempts.max(1))
        .collect()
}

/// 处理 UDP ASSOCIATE：绑中继 socket → 回其地址 → 跑中继循环直到 TCP 断开。
///
/// RFC 1928 要求 TCP 控制连接是 association 的生命周期锚点：TCP 一断，
/// 服务端必须回收该 association 的所有 UDP 状态。这里用 oneshot 通知中继循环退出。
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
