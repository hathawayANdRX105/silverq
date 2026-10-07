use silverq::config::settings::*;

fn write(tag: &str, content: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "silverq-cfg-{tag}-{}-{:?}.toml",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&p, content).unwrap();
    p
}

#[test]
fn missing_file_means_defaults() {
    let cfg = load(std::path::Path::new("/nonexistent/silverq.toml")).unwrap();
    assert_eq!(cfg.data_plane.fallback_attempts, 3);
    assert_eq!(
        cfg.scheduler.capacity,
        silverq::config::DEFAULT_ACTIVE_CAPACITY
    );
}

#[test]
fn parses_fallback_attempts() {
    let p = write("fb", "[data_plane]\nfallback_attempts = 5\n");
    let cfg = load(&p).unwrap();
    assert_eq!(cfg.data_plane.fallback_attempts, 5);
    let _ = std::fs::remove_file(p);
}

#[test]
fn unknown_keys_are_rejected() {
    // deny_unknown_fields：写错 key 必须大声失败，静默忽略会让人以为生效了
    let p = write("bad", "[scheduler]\ncappacity = 5\n");
    assert!(load(&p).is_err(), "拼写错误的 key 必须报错");
    let _ = std::fs::remove_file(p);
}

#[test]
fn full_example_parses() {
    let p = write(
        "full",
        r#"
[scheduler]
capacity = 8
interval_secs = 20
probe_urls = ["http://127.0.0.1:1/", "http://127.0.0.1:2/"]

[data_plane]
listen = "127.0.0.1:19999"
fallback_attempts = 2

[paths]
state = "/tmp/s.json"
"#,
    );
    let cfg = load(&p).unwrap();
    assert_eq!(cfg.scheduler.capacity, 8);
    // 多目标探测：列表顺序保留，供 measure() 逐个握手。
    assert_eq!(
        cfg.scheduler.probe_urls,
        vec!["http://127.0.0.1:1/", "http://127.0.0.1:2/"]
    );
    assert_eq!(cfg.data_plane.listen, "127.0.0.1:19999");
    assert_eq!(cfg.data_plane.fallback_attempts, 2);
    assert_eq!(cfg.paths.state, "/tmp/s.json");
    let _ = std::fs::remove_file(p);
}

#[test]
fn jev_disabled_by_default() {
    // 生产安全底线：没写 [jev] 就绝不能对进程外发任何请求。
    let cfg = load(std::path::Path::new("/nonexistent/silverq.toml")).unwrap();
    assert!(!cfg.jev.enabled);
    assert_eq!(cfg.jev.provider, "typesafe");
    assert!((cfg.jev.min_probability - 0.5).abs() < f64::EPSILON);
}

#[test]
fn parses_jev_section() {
    let p = write(
        "jev",
        "[jev]\nenabled = true\nprovider = \"compatible\"\n\
         base_url = \"http://127.0.0.1:8080\"\nmin_probability = 0.7\nttl_rounds = 2\n",
    );
    let cfg = load(&p).unwrap();
    assert!(cfg.jev.enabled);
    assert_eq!(cfg.jev.provider, "compatible");
    assert_eq!(cfg.jev.base_url, "http://127.0.0.1:8080");
    assert!((cfg.jev.min_probability - 0.7).abs() < f64::EPSILON);
    assert_eq!(cfg.jev.ttl_rounds, 2);
    let _ = std::fs::remove_file(p);
}

#[test]
fn unknown_jev_key_is_rejected() {
    // 拼错 key（enable 而非 enabled）必须大声失败：静默忽略会让人以为开着，
    // 实际决策层从未启动，fallback 记账也永远是空的。
    let p = write("jev-bad", "[jev]\nenable = true\n");
    assert!(load(&p).is_err(), "[jev] 拼写错误的 key 必须报错");
    let _ = std::fs::remove_file(p);
}

/// 调参夹紧：config-reload 重读的任意值都不会把调度打坏。
#[test]
fn tuning_validation_clamps() {
    use silverq::config::settings::RuntimeTuning;
    let t = RuntimeTuning {
        capacity: 9999,
        batch_size: 0,
        interval_secs: 1,
        timeout_ms: 10,
        concurrency: 0,
        timeout_penalty: -5.0,
        fallback_attempts: 100,
        retire_max_failures: 5000,
        retire_keep_alive_secs: 7200,
        retire_min_pool: 10,
        bw_interval_rounds: 0,
        bw_timeout_ms: 10,
        bw_max_bytes: 0,
        bw_penalty_per_efold_ms: -1.0,
    }
    .validated();
    assert_eq!(t.capacity, 50);
    assert_eq!(t.batch_size, 1);
    assert_eq!(t.interval_secs, 5);
    assert_eq!(t.timeout_ms, 500);
    assert_eq!(t.concurrency, 1);
    assert_eq!(t.timeout_penalty, 100.0);
    assert_eq!(t.retire_max_failures, 1000);
    assert_eq!(t.fallback_attempts, 10);
    assert_eq!(t.bw_interval_rounds, 1);
    assert_eq!(t.bw_timeout_ms, 1000);
    assert_eq!(t.bw_max_bytes, 16 * 1024);
    assert_eq!(t.bw_penalty_per_efold_ms, 0.0);
}
