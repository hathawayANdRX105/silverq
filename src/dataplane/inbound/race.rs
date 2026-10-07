//! 首字节竞速：按波次双发、判胜与路线/健康记账。

use crate::proxy::route::{Route, RouteCache, RouteOutcome};
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::health::attribute_runtime;
use super::runtime::SharedPool;
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
