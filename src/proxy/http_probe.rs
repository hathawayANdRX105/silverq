//! 通用 HTTP 探测原语：URL 解析、TLS 连接、有限字节 body 读取。
//!
//! 抽出来是因为延迟探测（meow `url_test`）与带宽探测共享同一条拨号+TLS 路径，
//! 但前者读完状态行就返回、丢弃 body，拿不到字节数。带宽探测需要自己的 body
//! 读取循环。两边对 URL 解析/TLS connector 的要求完全一致，放一处避免漂移。
//!
//! ponytail: TLS connector 用进程级 OnceLock 单例（rustls 的 ClientConfig
//! clone 很贵，每次探测重建会让 HTTPS 探测成为 CPU 热点）。
#![cfg(feature = "meow")]

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;
use tokio_rustls::TlsConnector;

/// 解析后的探测 URL。只支持 http/https + 显式或默认端口，够探测用。
#[derive(Debug, Clone)]
pub struct ProbeUrl {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// 含 query string 的路径（带宽探测的 `?bytes=N` 在这里）。
    pub path: String,
}

/// 解析 `scheme://host[:port]/path?query`。非法输入返回 None。
pub fn parse_probe_url(url: &str) -> Option<ProbeUrl> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        let r = url.strip_prefix("http://")?;
        (false, r)
    };
    if rest.is_empty() {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // IPv6 字面量 `[::1]:8080` 的冒号不能按 host:port 切分。
    let (host, port) = if let Some(rest6) = authority.strip_prefix('[') {
        let close = rest6.find(']')?;
        let host = &rest6[..close];
        let after = &rest6[close + 1..];
        let port = after
            .strip_prefix(':')
            .map(|p| p.parse().ok())
            .unwrap_or_else(|| if https { Some(443) } else { Some(80) })?;
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), if https { 443 } else { 80 }),
        }
    };
    Some(ProbeUrl {
        https,
        host,
        port,
        path: path.to_string(),
    })
}

/// TLS 握手。root store 用 webpki-roots（与 meow 的 url_test 同源）。
pub async fn tls_connect(
    host: &str,
    conn: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
) -> Option<
    tokio_rustls::client::TlsStream<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>,
> {
    let connector = tls_connector();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).ok()?;
    connector.connect(server_name, conn).await.ok()
}

fn tls_connector() -> TlsConnector {
    static CONNECTOR: std::sync::LazyLock<TlsConnector> = std::sync::LazyLock::new(|| {
        let root_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        // 显式指定 provider：开 aws-lc-rs 相关 feature 时 rustls::builder()
        // 会因两个 provider 同时编译而 panic（meow 的 ech-tls-tunnel 同款坑）。
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("rustls protocol versions are safe defaults")
        .with_root_certificates(root_store)
        .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    });
    CONNECTOR.clone()
}
/// 发 GET、读 body，**最多 `max_bytes` 字节后立即断开**，返回所读字节数。
///
/// 读完状态行+头部后开始计数 body。`Connection: close` 让服务端发完就关，
/// 我们也在达到 max_bytes 时主动断开——不读完整个响应，既省带宽又让
/// 「快但慢」的节点在有限预算内暴露真实速率。
pub async fn read_body_bytes<S>(mut stream: S, parsed: &ProbeUrl, max_bytes: u64) -> Option<u64>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let default_port = if parsed.https { 443 } else { 80 };
    // Host 头带非默认端口，虚拟主机路由才正确（与 meow 的 send_get_and_check 同理）。
    let host_header = if parsed.port == default_port {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: silverq-bw/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        parsed.path, host_header, env!("CARGO_PKG_VERSION")
    );
    stream.write_all(req.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    // 状态行 + 头部：读到 \r\n\r\n 为止。头部里可能有 Content-Length，
    // 但我们不信任它——节点可能虚报，实测字节数才是真相。
    let mut header_buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.ok()?;
        if n == 0 {
            return None; // 头部没结束就 EOF，非正常响应
        }
        header_buf.push(byte[0]);
        if header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if header_buf.len() > 8192 {
            return None; // 头部过大，不是正常下载端点
        }
    }
    // 只要状态行 2xx/3xx 就算可用下载（3xx 重定向不跟随，字节数 0 会被
    // 上层视为低带宽而非失败——重定向端点本就不是好的带宽探测点）。
    let status_ok = header_buf
        .windows(7)
        .next()
        .and_then(|w| std::str::from_utf8(w).ok())
        .and_then(|s| s.strip_prefix("HTTP/1."))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|c| (200..400).contains(&c));
    if !status_ok {
        return None;
    }

    let mut got: u64 = 0;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        if got >= max_bytes {
            break;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break; // 服务端发完/断开
        }
        got += n as u64;
    }
    // 一个字节都没下来：可能是空 body 的 204 被误当带宽端点，视为无效采样。
    (got > 0).then_some(got)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
