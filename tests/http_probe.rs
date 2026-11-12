//! 探测 URL 解析 + 状态行解析 + 速率换算（src/proxy/http_probe.rs）。
#![cfg(feature = "meow")]

use silverq::proxy::http_probe::{
    bytes_per_second, parse_probe_url, parse_status_code, read_body_bytes,
};
use std::time::Duration;

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

// ── 状态行解析：带宽探测的成败开关 ───────────────────────────────────
//
// 回归一个真 bug：原实现用 `header_buf.windows(7).next()` 取「前 7 字节」
// 再 `strip_prefix("HTTP/1.")`——7 字节窗口恰好只覆盖 `HTTP/1.` 本身，
// 剥完前缀剩空串，状态码恒为 None → **每次带宽探测都返回 None**，
// `bw_samples` 永远是 0，带宽维度从未进入过排序（实测 scores.json
// 26 个节点全为 `bw_samples: 0`）。后果就是「握手快但带宽只有
// 1Mbit」的节点一直霸占队首：延迟探测飞快、真实页面很慢。

#[test]
fn status_code_parses_2xx_and_3xx() {
    assert_eq!(
        parse_status_code(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n"),
        Some(200)
    );
    assert_eq!(
        parse_status_code(b"HTTP/1.1 204 No Content\r\n\r\n"),
        Some(204)
    );
    assert_eq!(
        parse_status_code(b"HTTP/1.1 302 Found\r\nLocation: /x\r\n\r\n"),
        Some(302)
    );
    // HTTP/1.0 也认（老端点）。
    assert_eq!(parse_status_code(b"HTTP/1.0 200 OK\r\n\r\n"), Some(200));
}

#[test]
fn status_code_rejects_non_http_and_missing_code() {
    // 4xx/5xx：探测端点报错，不是有效下载端点。
    assert_eq!(
        parse_status_code(b"HTTP/1.1 404 Not Found\r\n\r\n"),
        Some(404)
    );
    assert_eq!(
        parse_status_code(b"HTTP/1.1 503 Service Unavailable\r\n\r\n"),
        Some(503)
    );
    // 非 HTTP 开头（代理返回了别的协议）：不认。
    assert_eq!(parse_status_code(b"SSH-2.0-OpenSSH_9.6\r\n\r\n"), None);
    // 状态行只有版本、没有状态码：不认。
    assert_eq!(parse_status_code(b"HTTP/1.1\r\n\r\n"), None);
    assert_eq!(parse_status_code(b""), None);
}

// ── 速率换算：带宽维度要的量纲是 bytes/sec，不是字节数 ────────────────
//
// 同读满 max_bytes，2 秒读完是 256KB/s，8 秒读完是 64KB/s——只看字节数
// 两者完全一样，排序无从区分（这正是「快但慢节点霸占队首」的成因）。

#[test]
fn bytes_per_second_reflects_elapsed_time() {
    let bytes = 524_288u64;
    let fast = bytes_per_second(bytes, Duration::from_secs(2));
    let slow = bytes_per_second(bytes, Duration::from_secs(8));
    assert!(
        (fast - 262_144.0).abs() < 1.0,
        "2 秒读完 512KiB ≈ 256KB/s，got {fast}"
    );
    assert!(
        (slow - 65_536.0).abs() < 1.0,
        "8 秒读完 512KiB ≈ 64KB/s，got {slow}"
    );
    assert!(fast > slow * 3.9, "同字节数下耗时必须拉开速率差");
}

#[test]
fn bytes_per_second_never_divides_by_zero() {
    let rate = bytes_per_second(1024, Duration::ZERO);
    assert!(rate.is_finite(), "零耗时要兜底，不能产出 inf/NaN");
    assert!(rate > 0.0);
}

/// `read_body_bytes` 返回 `(字节数, 耗时)`——带宽维度要的是速率，
/// 不是字节数。返回形状变了却拿不到耗时就等于没修。
#[tokio::test]
async fn read_body_bytes_returns_bytes_and_elapsed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = [0u8; 1024];
        // 先把请求头读走（客户端写完就等应答，不会发更多）。
        let _ = sock.read(&mut req).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabcd")
            .await;
        let _ = sock.flush().await;
    });

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let parsed = parse_probe_url("http://example.com/x").unwrap();
    let got = read_body_bytes(&mut client, &parsed, 4096).await;

    let (bytes, elapsed) = got.expect("200 响应应产出采样");
    assert_eq!(bytes, 4, "应读到 body 的 4 字节");
    assert!(!elapsed.is_zero(), "耗时必须被测到，否则算不出速率");

    server.await.unwrap();
}
