//! 域名级路由缓存：直连 vs 代理，谁快用谁（慢触发竞速）。
//!
//! 触发模型：
//! 1. 某域名走代理的首响应超过 [`RACE_THRESHOLD`] → 标记 slow
//! 2. 下次访问该域名 → 并行竞速（直连 + 代理候选，TCP 先建连者胜）
//! 3. 竞速胜者的首响应参与滞回判定：新路线要快过旧路线一半才接管
//! 4. 缓存 TTL [`ROUTE_TTL`]，到期待重新观察
//!
//! 已知盲区（ponytail）：silverq 是隧道，看不到 TLS 层——「TCP 握手通、
//! TLS 被打断」的直连会被 TCP 竞速误判为优。缓解：直连侧失败（relay
//! 错误）计 direct_fails，连续 2 次移除缓存回退代理；TTL 只有 5 分钟。
//! TUN 数据面暂未接入：meow-tunnel 的首响应信号在引擎内部，需 meow 侧
//! 配合后才能把同样的机制搬到 TUN 路径（SOCKS 路径先行）。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 代理首响应超过该阈值 → 标记 slow，下次访问触发竞速。
pub const RACE_THRESHOLD: Duration = Duration::from_secs(2);
/// 路由缓存 TTL。
pub const ROUTE_TTL: Duration = Duration::from_secs(300);
/// 直连连续失败多少次后移除缓存（TLS 盲区缓解：TCP 通但 TLS 死的直连
/// 会在应用层反复失败，靠失败计数把它踢出）。
const DIRECT_FAILS_TO_DROP: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    Direct,
    Proxy,
}

/// 单连接结果（来自 relay 的首响应观测）。
pub enum RouteOutcome {
    /// 有数据响应，d = 首响应耗时
    Responded(Duration),
    /// 连接建立但无数据（对端正常关闭/客户端早退）——中性，仅续期
    Neutral,
    /// 连接失败（dial 失败 / 黑洞超时 / 读写错误）
    Failed,
}

/// 缓存决策。None 表示目标不在缓存管辖内（走调用方默认路径）。
pub enum Decision {
    /// 按缓存走直连
    Direct,
    /// 按缓存走代理
    Proxy,
    /// 直连与代理并行竞速，TCP 先建连者胜
    Race,
}

struct Entry {
    route: Route,
    /// 当前路线最近一次首响应耗时（滞回基准）
    last_fr: Duration,
    /// 当前路线慢 → 下次访问触发竞速
    slow: bool,
    direct_fails: u32,
    until: Instant,
}

#[derive(Default)]
pub struct RouteCache {
    entries: Mutex<HashMap<String, Entry>>,
}

impl RouteCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn decide(&self, host: &str) -> Decision {
        let mut m = self.entries.lock();
        let Some(e) = m.get_mut(host) else {
            return Decision::Proxy; // 无缓存：单路代理（免费池语义）
        };
        if Instant::now() >= e.until {
            m.remove(host);
            return Decision::Proxy;
        }
        match e.route {
            Route::Direct if e.slow => Decision::Race, // 直连路线也慢 → 竞速换路线
            Route::Direct => Decision::Direct,
            Route::Proxy if e.slow => Decision::Race,
            Route::Proxy => Decision::Proxy,
        }
    }

    pub fn record(&self, host: &str, route: Route, outcome: RouteOutcome) {
        let mut m = self.entries.lock();
        let now = Instant::now();
        let e = m.entry(host.to_string()).or_insert(Entry {
            route,
            last_fr: Duration::ZERO,
            slow: false,
            direct_fails: 0,
            until: now + ROUTE_TTL,
        });

        match outcome {
            RouteOutcome::Failed => {
                if route == Route::Direct {
                    e.direct_fails += 1;
                    if e.direct_fails >= DIRECT_FAILS_TO_DROP {
                        // 直连连续失败：移除缓存回退代理（TLS 盲区缓解）
                        m.remove(host);
                        return;
                    }
                    e.slow = true; // 直连失败一次：下次竞速验证
                } else {
                    e.slow = true; // 代理失败：下次竞速
                }
                e.until = now + ROUTE_TTL;
            }
            RouteOutcome::Neutral => {
                e.until = now + ROUTE_TTL;
            }
            RouteOutcome::Responded(fr) => {
                if e.route != route {
                    // 竞速换路线：滞回——新路线要快过旧路线一半才接管
                    if fr * 2 < e.last_fr {
                        e.route = route;
                        e.last_fr = fr;
                        e.slow = false;
                        e.direct_fails = 0;
                    } else {
                        e.slow = false; // 没快到值得切：保留旧路线
                    }
                } else {
                    e.last_fr = fr;
                    e.slow = fr > RACE_THRESHOLD;
                    e.direct_fails = 0;
                }
                e.until = now + ROUTE_TTL;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn responded(ms: u64) -> RouteOutcome {
        RouteOutcome::Responded(Duration::from_millis(ms))
    }

    #[test]
    fn first_visit_marks_slow_on_threshold() {
        let c = RouteCache::new();
        assert!(matches!(c.decide("a.com"), Decision::Proxy));
        c.record("a.com", Route::Proxy, responded(500));
        assert!(matches!(c.decide("a.com"), Decision::Proxy)); // 快 → 不竞速
        c.record("a.com", Route::Proxy, responded(3000));
        assert!(matches!(c.decide("a.com"), Decision::Race)); // 3s > 2s → 竞速
    }

    #[test]
    fn race_switch_commits_only_with_hysteresis() {
        let c = RouteCache::new();
        c.record("g.com", Route::Proxy, responded(3000)); // slow
        assert!(matches!(c.decide("g.com"), Decision::Race));
        // 竞速直连 700ms：2×700 < 3000 → 接管
        c.record("g.com", Route::Direct, responded(700));
        assert!(matches!(c.decide("g.com"), Decision::Direct));
    }

    #[test]
    fn race_switch_reverts_without_hysteresis() {
        let c = RouteCache::new();
        c.record("h.com", Route::Proxy, responded(3000)); // slow, last_fr=3s
                                                          // 竞速直连 2s：2×2000 = 4000 不小于 3000 → 不接管，保留代理
        c.record("h.com", Route::Direct, responded(2000));
        assert!(matches!(c.decide("h.com"), Decision::Proxy));
    }

    #[test]
    fn direct_failures_drop_entry() {
        let c = RouteCache::new();
        c.record("d.com", Route::Proxy, responded(3000)); // slow
        c.record("d.com", Route::Direct, RouteOutcome::Failed);
        assert!(!matches!(c.decide("d.com"), Decision::Direct));
        c.record("d.com", Route::Direct, RouteOutcome::Failed);
        // 连续 2 次失败 → 移除 → 回代理语义
        assert!(matches!(c.decide("d.com"), Decision::Proxy));
    }

    #[test]
    fn proxy_failure_marks_race() {
        let c = RouteCache::new();
        c.record("p.com", Route::Proxy, responded(500));
        assert!(matches!(c.decide("p.com"), Decision::Proxy));
        c.record("p.com", Route::Proxy, RouteOutcome::Failed);
        assert!(matches!(c.decide("p.com"), Decision::Race));
    }

    #[test]
    fn ttl_expiry_returns_to_proxy() {
        let c = RouteCache::new();
        c.record("t.com", Route::Direct, responded(100));
        assert!(matches!(c.decide("t.com"), Decision::Direct));
        // 手动过期（同模块可触私有字段）
        let mut m = c.entries.lock();
        m.get_mut("t.com").unwrap().until = Instant::now() - Duration::from_secs(1);
        drop(m);
        assert!(matches!(c.decide("t.com"), Decision::Proxy));
    }
}
