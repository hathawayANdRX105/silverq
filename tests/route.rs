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
