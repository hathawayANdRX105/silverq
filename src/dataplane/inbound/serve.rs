//! 单连接处理：协议识别、直连/竞速/候选链决策与中继派发。

use crate::proxy::meow::Registry;
use crate::proxy::route::{Route, RouteOutcome};
use meow_common::Metadata;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::direct::{dial_direct, single_budget_direct, DirectAttempt};
use super::health::{attribute_runtime, reorder_recent_failures, RECENT_FAILURE_COOLDOWN};
use super::policy::{is_local_target, pick_candidates, route_cache_eligible};
use super::protocol::{read_http_connect_target, read_socks5_target, socks5_greeting};
use super::race::{race_relay, replay_race_safe, RaceCtx, RaceOutcome, SideFut, SideSpec};
use super::relay::{relay, RelayFail, RelayOutcome, RouteRecorder};
use super::runtime::{Cmd, DialTuning, InboundRuntime, Proto};
use super::udp::handle_udp_associate;
/// handle_one 的共享上下文（参数打包：7 个以上就被 clippy 拦了）。
pub(super) struct ConnCtx<'a> {
    pub(super) registry: &'a Registry,
    pub(super) runtime: &'a InboundRuntime,
}

pub(super) async fn handle_one(
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
