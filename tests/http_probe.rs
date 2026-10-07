//! 探测 URL 解析（src/proxy/http_probe.rs 的 parse_probe_url 测试）。
#![cfg(feature = "meow")]

use silverq::proxy::http_probe::parse_probe_url;

#[test]
fn parse_https_default_port() {
    let u = parse_probe_url("https://speed.cloudflare.com/__down?bytes=524288").unwrap();
    assert_eq!(
        (u.https, u.host.as_str(), u.port, u.path.as_str()),
        (true, "speed.cloudflare.com", 443, "/__down?bytes=524288")
    );
}

#[test]
fn parse_http_explicit_port_and_no_path() {
    let u = parse_probe_url("http://127.0.0.1:8080").unwrap();
    assert_eq!(
        (u.https, u.host.as_str(), u.port, u.path.as_str()),
        (false, "127.0.0.1", 8080, "/")
    );
}

#[test]
fn parse_ipv6() {
    let u = parse_probe_url("http://[::1]:8080/x").unwrap();
    assert_eq!(
        (u.host.as_str(), u.port, u.path.as_str()),
        ("::1", 8080, "/x")
    );
}

#[test]
fn parse_rejects_garbage() {
    assert!(parse_probe_url("ftp://x").is_none());
    assert!(parse_probe_url("example.com").is_none());
    assert!(parse_probe_url("https://").is_none());
}
