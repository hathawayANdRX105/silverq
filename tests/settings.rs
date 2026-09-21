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
