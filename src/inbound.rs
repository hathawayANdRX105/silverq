//! 数据面：最小 SOCKS5/HTTP-CONNECT 混合 inbound。
//! 转发目标 = 调度循环维护的"当前 top-N 选择"，经 meow adapter 直连。
//! lift 自己就是 selector；meow 只负责协议与连接。
#![cfg(feature = "meow")]

use crate::meow::Registry;
use meow_common::Metadata;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 共享选择状态：调度循环写，inbound 每连接读。
pub type SharedSelection = Arc<tokio::sync::RwLock<Vec<String>>>;

pub async fn run(
    listener_addr: &str,
    registry: Registry,
    selection: SharedSelection,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(listener_addr).await?;
    tracing::info!(addr = listener_addr, "lift inbound listening (SOCKS5/HTTP-CONNECT)");

    loop {
        let (socket, peer) = listener.accept().await?;
        let registry = registry.clone();
        let selection = selection.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_one(socket, peer, &registry, &selection).await {
                tracing::debug!(peer = %peer, "{e}");
            }
        });
    }
}

#[derive(PartialEq)]
enum Proto {
    Socks5,
    Http,
}

struct Target {
    host: String,
    port: u16,
    proto: Proto,
}

async fn handle_one(
    mut socket: TcpStream,
    peer: SocketAddr,
    registry: &Registry,
    selection: &SharedSelection,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut peek = [0u8; 1];
    socket
        .read_exact(&mut peek)
        .await
        .map_err(|e| format!("head: {e}"))?;

    let target = if peek[0] == 0x05 {
        // SOCKS5 握手协商：greeting(VER already read, NMETHODS, METHODS...) → 选择 no-auth
        socks5_greeting(&mut socket).await?;
        read_socks5_target(&mut socket).await?
    } else if peek[0].is_ascii_uppercase() {
        // HTTP 方法行（CONNECT / GET / ...）；首字节已被读掉，传给解析器补回
        read_http_connect_target(&mut socket, peek[0]).await?
    } else {
        socket
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await
            .ok();
        return Ok(());
    };

    if target.host.is_empty() {
        return Ok(()); // 非 CONNECT 命令，协议层已应答
    }

    // 协议应答
    match target.proto {
        // SOCKS5 成功应答必须是完整 10 字节：VER REP RSV ATYP BND.ADDR(4) BND.PORT(2)
        Proto::Socks5 => {
            socket
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await?
        }
        Proto::Http => {
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?
        }
    }

    // 按当前选择顺序逐个尝试（best → 次优），dial 失败自动 fallback
    let order = selection.read().await.clone();
    let metadata = Metadata {
        network: meow_common::Network::Tcp,
        host: target.host.clone().into(),
        dst_port: target.port,
        ..Default::default()
    };

    // 先在无锁情况下把候选 adapter 克隆出来，避免 guard 跨 await
    let candidates: Vec<_> = {
        let guard = registry.read();
        order
            .iter()
            .filter_map(|tag| guard.get(tag).cloned())
            .collect()
    };

    let mut conn = None;
    for adapter in &candidates {
        match adapter.dial_tcp(&metadata).await {
            Ok(c) => {
                conn = Some(c);
                break;
            }
            Err(e) => {
                tracing::debug!(tag = adapter.name(), "dial failed: {e}");
            }
        }
    }
    let Some(conn) = conn else {
        if target.proto == Proto::Http {
            socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")
                .await
                .ok();
        }
        tracing::warn!(
            target = %format!("{}/{}", target.host, target.port),
            "no live adapter in selection"
        );
        return Ok(());
    };
    let _ = peer;

    // 双向拷贝（meow 的 ProxyConn 就是 tokio AsyncRead/AsyncWrite）
    let (mut cr, mut cw) = tokio::io::split(socket);
    let (mut pr, mut pw) = tokio::io::split(conn);
    let _ = tokio::join!(
        tokio::io::copy(&mut cr, &mut pw),
        tokio::io::copy(&mut pr, &mut cw)
    );
    Ok(())
}

/// SOCKS5 握手协商：VER 已被调用方读掉，这里读 NMETHODS + METHODS，
/// 应答 no-auth（0x00）。不支持认证——本地回环端口，鉴权交给绑定地址。
async fn socks5_greeting(
    socket: &mut TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut n = [0u8; 1];
    socket.read_exact(&mut n).await?;
    let mut methods = vec![0u8; n[0] as usize];
    if !methods.is_empty() {
        socket.read_exact(&mut methods).await?;
    }
    if !methods.contains(&0x00) {
        socket.write_all(&[0x05, 0xFF]).await.ok(); // 无可接受方法
        return Err("socks5: client requires auth, only no-auth supported".into());
    }
    socket.write_all(&[0x05, 0x00]).await?;
    Ok(())
}

/// 读 SOCKS5 请求（VER CMD RSV ATYP + 地址 + 端口）。
async fn read_socks5_target(
    socket: &mut TcpStream,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    let mut head = [0u8; 4];
    socket.read_exact(&mut head).await?;
    if head[0] != 0x05 || head[1] != 0x01 {
        // 只支持 CONNECT；回 0x07 command not supported（完整 10 字节）
        socket
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .ok();
        return Ok(Target {
            host: String::new(),
            port: 0,
            proto: Proto::Socks5,
        });
    }

    let mut host_bytes: Vec<u8> = Vec::new();
    let port = match head[3] {
        0x01 => {
            let mut b = [0u8; 6];
            socket.read_exact(&mut b).await?;
            host_bytes.extend_from_slice(&b[..4]);
            u16::from_be_bytes([b[4], b[5]])
        }
        0x03 => {
            let mut len = [0u8; 1];
            socket.read_exact(&mut len).await?;
            host_bytes.resize(len[0] as usize, 0);
            socket.read_exact(&mut host_bytes).await?;
            let mut p = [0u8; 2];
            socket.read_exact(&mut p).await?;
            u16::from_be_bytes(p)
        }
        0x04 => {
            let mut b = [0u8; 18];
            socket.read_exact(&mut b).await?;
            host_bytes.extend_from_slice(&b[..16]);
            u16::from_be_bytes([b[16], b[17]])
        }
        _ => {
            socket
                .write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .ok();
            return Ok(Target {
                host: String::new(),
                port: 0,
                proto: Proto::Socks5,
            });
        }
    };

    let host = if host_bytes.len() == 4 {
        host_bytes
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(".")
    } else if host_bytes.len() == 16 {
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&host_bytes);
        std::net::Ipv6Addr::from(addr).to_string()
    } else {
        String::from_utf8_lossy(&host_bytes).into_owned()
    };

    Ok(Target {
        host,
        port,
        proto: Proto::Socks5,
    })
}

async fn read_http_connect_target(
    socket: &mut TcpStream,
    first_byte: u8,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    // 首字节已被调用方消耗，补回后读完方法行
    let mut line = String::from(first_byte as char);
    let mut b = [0u8; 1];
    loop {
        socket
            .read_exact(&mut b)
            .await
            .map_err(|e| e.to_string())?;
        line.push(b[0] as char);
        if b[0] == b'\n' {
            break;
        }
        if line.len() > 1024 {
            return Ok(Target {
                host: String::new(),
                port: 0,
                proto: Proto::Http,
            });
        }
    }

    // 读完剩余头部，直到空行（CRLF CRLF）。
    // 必须精确停在空行后：残留字节会被拷进隧道，污染客户端 TLS ClientHello。
    let mut header_bytes = 0usize;
    loop {
        let mut cur = Vec::with_capacity(64);
        loop {
            socket
                .read_exact(&mut b)
                .await
                .map_err(|e| e.to_string())?;
            header_bytes += 1;
            if b[0] == b'\n' {
                break;
            }
            cur.push(b[0]);
            if header_bytes > 16 * 1024 {
                return Err("http connect: headers too large".into());
            }
        }
        // 空行（只剩 \r 或什么都没有）= 头部结束
        if cur.iter().all(|c| *c == b'\r') {
            break;
        }
        if header_bytes > 16 * 1024 {
            return Err("http connect: headers too large".into());
        }
    }

    let parts: Vec<&str> = line.trim_end().split_whitespace().collect();
    if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("CONNECT") {
        socket
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            .await
            .ok();
        return Ok(Target {
            host: String::new(),
            port: 0,
            proto: Proto::Http,
        });
    }

    let (host, port) = match parts[1].rsplit_once(':') {
        Some((h, p)) => (
            h.trim_end_matches(']').to_string(),
            p.parse::<u16>().map_err(|_| "bad port")?,
        ),
        None => (parts[1].to_string(), 443),
    };

    Ok(Target {
        host,
        port,
        proto: Proto::Http,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 建一对本地连接：返回 (客户端侧, 服务端侧)。
    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr);
        let server = listener.accept();
        let (client, server) = tokio::join!(client, server);
        (client.unwrap(), server.unwrap().0)
    }

    #[tokio::test]
    async fn socks5_greeting_selects_no_auth() {
        let (mut client, mut server) = pair().await;
        // VER 已被 dispatch 读掉；客户端发 NMETHODS=1, METHOD=no-auth
        client.write_all(&[0x01, 0x00]).await.unwrap();

        socks5_greeting(&mut server).await.unwrap();

        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00], "必须回 VER=5 METHOD=no-auth");
    }

    #[tokio::test]
    async fn socks5_greeting_rejects_auth_only_client() {
        let (mut client, mut server) = pair().await;
        // 只提供 username/password(0x02)，不含 no-auth
        client.write_all(&[0x01, 0x02]).await.unwrap();

        assert!(socks5_greeting(&mut server).await.is_err());

        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0xFF], "无可接受方法必须回 0xFF");
    }

    #[tokio::test]
    async fn socks5_parses_domain_request() {
        let (mut client, mut server) = pair().await;
        let host = b"example.com";
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host);
        req.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&req).await.unwrap();

        let target = read_socks5_target(&mut server).await.unwrap();
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 443);
    }

    #[tokio::test]
    async fn socks5_parses_ipv4_request() {
        let (mut client, mut server) = pair().await;
        let mut req = vec![0x05, 0x01, 0x00, 0x01, 1, 1, 1, 1];
        req.extend_from_slice(&80u16.to_be_bytes());
        client.write_all(&req).await.unwrap();

        let target = read_socks5_target(&mut server).await.unwrap();
        assert_eq!(target.host, "1.1.1.1");
        assert_eq!(target.port, 80);
    }

    /// 回归：曾经 drain 循环碰到第一个 CRLF 就停，剩余头部残留在 socket 里
    /// 被拷进隧道，污染客户端 TLS ClientHello。头部必须精确读到空行为止。
    #[tokio::test]
    async fn http_connect_drains_headers_exactly() {
        let (mut client, mut server) = pair().await;
        // 首字节 'C' 由 dispatch 消耗，这里从 'O' 开始
        client
            .write_all(
                b"ONNECT example.com:443 HTTP/1.1\r\n\
                  Host: example.com:443\r\n\
                  Proxy-Connection: Keep-Alive\r\n\
                  \r\n",
            )
            .await
            .unwrap();
        // 头部之后立刻写隧道数据（模拟 TLS ClientHello 首字节）
        client.write_all(b"\x16\x03\x01TUNNEL").await.unwrap();

        let target = read_http_connect_target(&mut server, b'C').await.unwrap();
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 443);

        // 关键断言：socket 里剩下的必须正好是隧道数据，没有残留头部字节
        let mut rest = [0u8; 9];
        server.read_exact(&mut rest).await.unwrap();
        assert_eq!(
            &rest,
            b"\x16\x03\x01TUNNEL",
            "头部残留会污染隧道数据"
        );
    }

    #[tokio::test]
    async fn http_non_connect_method_rejected() {
        let (mut client, mut server) = pair().await;
        client
            .write_all(b"ET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();

        let target = read_http_connect_target(&mut server, b'G').await.unwrap();
        assert!(target.host.is_empty(), "非 CONNECT 不应产生转发目标");

        let mut buf = vec![0u8; 32];
        let n = client.read(&mut buf).await.unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains("405"),
            "应回 405"
        );
    }
}
