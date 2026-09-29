//! 域名级路由缓存：直连优先，缓存记的是「曾经成功的那条路线」（可能是代理）。
//!
//! 触发模型：
//! 1. 缓存未命中 → 直连优先；直连拨不通回退代理候选链（调用方 fallback）
//! 2. 命中 → 按缓存里的路线走，谁上次成功听谁的
//! 3. 走代理的首响应超过 [`RACE_THRESHOLD`] → 标记 slow → 下次并行竞速
//!    （直连 + 代理候选，TCP 先建连者胜）
//! 4. 换路线走滞回：新路线要快过旧路线一半才接管；旧路线从没测通过
//!    （无耗时基线）时不享受滞回，成功的路线直接接管
//! 5. 缓存 TTL [`ROUTE_TTL`]，到期待重新观察
//!
//! 已知盲区（ponytail）：silverq 是隧道，看不到 TLS 层——「TCP 握手通、
//! TLS 被打断」的直连会被 TCP 竞速误判为优。缓解：直连侧失败（relay
//! 错误 / 拨号失败）计 direct_fails，连续 2 次移除条目回未命中，代理靠
//! fallback 链兜住；TTL 只有 5 分钟。TUN 数据面暂未接入：meow-tunnel 的首响应信号在引擎内部，需 meow 侧
//! 配合后才能把同样的机制搬到 TUN 路径（SOCKS 路径先行）。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 代理首响应超过该阈值 → 标记 slow，下次访问触发竞速。
pub const RACE_THRESHOLD: Duration = Duration::from_secs(2);
/// 路由缓存 TTL。
pub const ROUTE_TTL: Duration = Duration::from_secs(300);
/// 直连连续失败多少次后把条目切到代理路线（TLS 盲区缓解：TCP 通但 TLS 死
/// 的直连会在应用层反复失败，靠失败计数强制收敛到代理，不再回未命中重试
/// 直连——那会对这类域名形成永久的直连预算循环）。
const DIRECT_FAILS_TO_SWITCH: u32 = 2;
/// 缓存条目上限。过期只在 decide/record 访问该 host 时惰性清理，浏览器
/// 流量里大量一次性域名不会被复查——不主动清扫则长跑进程内存只涨不消。
const MAX_ENTRIES: usize = 8192;

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

    /// 路线决策：命中按缓存走，未命中/过期回直连优先。
    ///
    /// 未命中返回 [`Decision::Direct`] —— 直连失败由调用方回退代理候选链，
    /// 成功的那条路线会被 [`RouteCache::record`] 写回缓存，下次直接照走。
    pub fn decide(&self, host: &str) -> Decision {
        let mut m = self.entries.lock();
        let Some(e) = m.get_mut(host) else {
            return Decision::Direct; // 无缓存：直连优先，失败再走代理候选链
        };
        if Instant::now() >= e.until {
            m.remove(host);
            return Decision::Direct;
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
        // 超限清扫：先丢过期，仍超限整体清空。缓存是建议性的——最坏代价
        // 只是几个域名重走一次观察期，换来内存有界。
        if m.len() >= MAX_ENTRIES {
            m.retain(|_, e| e.until > now);
            if m.len() >= MAX_ENTRIES {
                m.clear();
            }
        }
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
                    if e.direct_fails >= DIRECT_FAILS_TO_SWITCH {
                        // 直连连续失败：条目切到代理路线（TLS 盲区缓解）。不能
                        // 移除回未命中——未命中又优先直连，对「TCP 通但 TLS 被
                        // 吞」的域名形成永久直连循环（#21）。切到代理后下次
                        // 请求直接走代理链，成功即把路线写回缓存，形成收敛。
                        e.route = Route::Proxy;
                        e.direct_fails = 0;
                        e.slow = false;
                        e.until = now + ROUTE_TTL;
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
                    // 竞速/兜底换路线：滞回——新路线要快过旧路线一半才接管。
                    // 旧路线从没测通过（无耗时基线）时没有可保护的成绩，
                    // 谁成功听谁的：直连失败后代理兜底成功就得立刻记住代理，
                    // 否则缓存会继续指一条刚拨不通的路线。
                    if e.last_fr.is_zero() || fr * 2 < e.last_fr {
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
        assert!(matches!(c.decide("a.com"), Decision::Direct)); // 无缓存 → 直连优先
        c.record("a.com", Route::Proxy, responded(500));
        assert!(matches!(c.decide("a.com"), Decision::Proxy)); // 快 → 不竞速
        c.record("a.com", Route::Proxy, responded(3000));
        assert!(matches!(c.decide("a.com"), Decision::Race)); // 3s > 2s → 竞速
    }

    /// 未命中即直连优先：这是 fallback 链的第一跳。
    #[test]
    fn cache_miss_prefers_direct() {
        let c = RouteCache::new();
        assert!(matches!(c.decide("never-seen.com"), Decision::Direct));
    }

    /// 直连拨不通、代理兜底成功 → 一次请求就把缓存改写成代理路线，
    /// 下次不再白烧一次直连超时。
    #[test]
    fn direct_failure_then_proxy_success_caches_proxy() {
        let c = RouteCache::new();
        assert!(matches!(c.decide("f.com"), Decision::Direct));
        c.record("f.com", Route::Direct, RouteOutcome::Failed);
        c.record("f.com", Route::Proxy, responded(400));
        assert!(matches!(c.decide("f.com"), Decision::Proxy));
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

    /// 连续直连失败后条目切到代理路线，不回未命中（#21 回归）：
    /// 「TCP 通、TLS 被吞」的域名从没写过代理路线，若踢条目回未命中，
    /// 未命中又优先直连，每次访问都重烧一轮直连预算，永不收敛。
    #[test]
    fn direct_failures_switch_entry_to_proxy() {
        let c = RouteCache::new();
        assert!(matches!(c.decide("d.com"), Decision::Direct)); // 未命中 → 直连观察
        c.record("d.com", Route::Direct, RouteOutcome::Failed);
        assert!(matches!(c.decide("d.com"), Decision::Race)); // 一次失败 → 竞速验证
        c.record("d.com", Route::Direct, RouteOutcome::Failed);
        assert!(matches!(c.decide("d.com"), Decision::Proxy)); // 两次 → 切代理，收敛
    }

    #[test]
    fn proxy_failure_marks_race() {
        let c = RouteCache::new();
        c.record("p.com", Route::Proxy, responded(500));
        assert!(matches!(c.decide("p.com"), Decision::Proxy));
        c.record("p.com", Route::Proxy, RouteOutcome::Failed);
        assert!(matches!(c.decide("p.com"), Decision::Race));
    }

    /// 过期后回未命中语义（直连优先）。用代理条目过期来断言，才证得出
    /// 「条目确实被作废」——直连条目过期前后同为 Direct，断言不出差别。
    #[test]
    fn ttl_expiry_returns_to_direct_first() {
        let c = RouteCache::new();
        c.record("t.com", Route::Proxy, responded(100));
        assert!(matches!(c.decide("t.com"), Decision::Proxy));
        // 手动过期（同模块可触私有字段）
        let mut m = c.entries.lock();
        m.get_mut("t.com").unwrap().until = Instant::now() - Duration::from_secs(1);
        drop(m);
        assert!(matches!(c.decide("t.com"), Decision::Direct));
        assert!(c.entries.lock().is_empty(), "过期条目应在 decide 时清掉");
    }

    /// 一次性域名（浏览器流量）不会被复查 → 惰性过期覆盖不到。
    /// 不清扫则长跑进程内存只涨不消：超限必须主动丢。
    #[test]
    fn entries_stay_bounded_under_one_shot_hosts() {
        let c = RouteCache::new();
        for i in 0..MAX_ENTRIES + 64 {
            c.record(&format!("one-shot-{i}.com"), Route::Proxy, responded(10));
        }
        assert!(
            c.entries.lock().len() <= MAX_ENTRIES,
            "缓存条目数必须不超过上限"
        );
    }

    /// 超限清扫优先丢过期条目：活的观察期条目不该被整体清空误伤。
    #[test]
    fn purge_prefers_expired_entries() {
        let c = RouteCache::new();
        for i in 0..MAX_ENTRIES {
            c.record(&format!("h{i}.com"), Route::Proxy, responded(10));
        }
        // 全设为过期，再写入一个触发清扫
        {
            let mut m = c.entries.lock();
            let past = Instant::now() - Duration::from_secs(1);
            for e in m.values_mut() {
                e.until = past;
            }
        }
        c.record("fresh.com", Route::Proxy, responded(10));
        let m = c.entries.lock();
        assert_eq!(m.len(), 1, "过期条目应被清掉，只剩新写入的");
        assert!(m.contains_key("fresh.com"));
    }
}
