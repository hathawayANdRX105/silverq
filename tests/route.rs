use silverq::proxy::route::{Decision, Route, RouteCache, RouteOutcome};
use std::time::Duration;

#[test]
fn repeatedly_slow_proxy_races_once_then_reuses_last_working_route() {
    let routes = RouteCache::new();
    routes.record(
        "slow.example",
        Route::Proxy,
        RouteOutcome::Responded(Duration::from_secs(3)),
    );

    assert!(matches!(routes.decide("slow.example"), Decision::Race));
    assert!(matches!(routes.decide("slow.example"), Decision::Proxy));

    routes.record(
        "slow.example",
        Route::Proxy,
        RouteOutcome::Responded(Duration::from_secs(3)),
    );
    assert!(matches!(routes.decide("slow.example"), Decision::Proxy));

    routes.record("slow.example", Route::Proxy, RouteOutcome::Failed);
    assert!(matches!(routes.decide("slow.example"), Decision::Race));
}

#[test]
fn failed_direct_route_yields_to_successful_proxy_even_when_slower() {
    let routes = RouteCache::new();
    routes.record(
        "broken.example",
        Route::Direct,
        RouteOutcome::Responded(Duration::from_millis(100)),
    );
    routes.record("broken.example", Route::Direct, RouteOutcome::Failed);
    routes.record(
        "broken.example",
        Route::Proxy,
        RouteOutcome::Responded(Duration::from_millis(500)),
    );
    assert!(matches!(routes.decide("broken.example"), Decision::Proxy));
}

use silverq::proxy::route::MAX_ENTRIES;

fn responded(ms: u64) -> RouteOutcome {
    RouteOutcome::Responded(Duration::from_millis(ms))
}

#[test]
fn first_visit_marks_slow_on_threshold() {
    let c = RouteCache::new();
    assert!(matches!(c.decide("a.com"), Decision::Race)); // 无缓存 → 并行竞速
    c.record("a.com", Route::Proxy, responded(500));
    assert!(matches!(c.decide("a.com"), Decision::Proxy)); // 快 → 不竞速
    c.record("a.com", Route::Proxy, responded(3000));
    assert!(matches!(c.decide("a.com"), Decision::Race)); // 3s > 2s → 竞速
}

/// 未命中即并行竞速：直连 ‖ 第一候选，首字节先到者胜（#23）。
#[test]
fn cache_miss_races() {
    let c = RouteCache::new();
    assert!(matches!(c.decide("never-seen.com"), Decision::Race));
}

/// 直连拨不通、代理兜底成功 → 一次请求就把缓存改写成代理路线，
/// 下次不再白烧一次直连超时。
#[test]
fn direct_failure_then_proxy_success_caches_proxy() {
    let c = RouteCache::new();
    assert!(matches!(c.decide("f.com"), Decision::Race));
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
/// 每次访问又把这条直接送进竞速重烧一轮拨号，永不收敛。
#[test]
fn direct_failures_switch_entry_to_proxy() {
    let c = RouteCache::new();
    assert!(matches!(c.decide("d.com"), Decision::Race)); // 未命中 → 竞速观察
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

/// 过期后回未命中语义（并行竞速）。用代理条目过期来断言，才证得出
/// 「条目确实被作废」——过期前后 Proxy ≠ Race 的差值就是作废本身。
#[test]
fn ttl_expiry_returns_to_race() {
    let c = RouteCache::new();
    c.record("t.com", Route::Proxy, responded(100));
    assert!(matches!(c.decide("t.com"), Decision::Proxy));
    c.force_expire("t.com"); // 白盒钩子：TTL 180s 无法在单测里等
    assert!(matches!(c.decide("t.com"), Decision::Race));
    assert_eq!(c.entry_count(), 0, "过期条目应在 decide 时清掉");
}

/// 一次性域名（浏览器流量）不会被复查 → 惰性过期覆盖不到。
/// 不清扫则长跑进程内存只涨不消：超限必须主动丢。
#[test]
fn entries_stay_bounded_under_one_shot_hosts() {
    let c = RouteCache::new();
    for i in 0..MAX_ENTRIES + 64 {
        c.record(&format!("one-shot-{i}.com"), Route::Proxy, responded(10));
    }
    assert!(c.entry_count() <= MAX_ENTRIES, "缓存条目数必须不超过上限");
}

/// 超限清扫优先丢过期条目：活的观察期条目不该被整体清空误伤。
#[test]
fn purge_prefers_expired_entries() {
    let c = RouteCache::new();
    for i in 0..MAX_ENTRIES {
        c.record(&format!("h{i}.com"), Route::Proxy, responded(10));
    }
    // 全设为过期，再写入一个触发清扫
    c.force_expire_all();
    c.record("fresh.com", Route::Proxy, responded(10));
    assert_eq!(c.entry_count(), 1, "过期条目应被清掉，只剩新写入的");
    assert!(
        matches!(c.decide("fresh.com"), Decision::Proxy),
        "新写入的条目必须还在"
    );
}
