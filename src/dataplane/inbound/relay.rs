//! 双向中继：首字节当下记账与节点 HP 归因。

use crate::proxy::route::{Route, RouteCache, RouteOutcome};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::health::attribute_runtime;
use super::runtime::SharedPool;
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
