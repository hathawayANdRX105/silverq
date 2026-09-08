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
        read_socks5_target(&mut socket, true).await?
    } else if peek[0] == b'H' {
        read_http_connect_target(&mut socket).await?
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
        Proto::Socks5 => socket.write_all(b"\x05\x00\x00\x00").await?,
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

/// first_byte_consumed: 调用方已消耗首字节 0x05。
async fn read_socks5_target(
    socket: &mut TcpStream,
    first_byte_consumed: bool,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    let mut head = [0u8; 4];
    if !first_byte_consumed {
        socket.read_exact(&mut head).await?;
    } else {
        head[0] = 0x05;
        socket.read_exact(&mut head[1..]).await?;
    }
    if head[0] != 0x05 || head[1] != 0x01 {
        socket.write_all(&[0x05, 0x07, 0x00, 0x00]).await.ok();
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
            socket.write_all(&[0x05, 0x08, 0x00, 0x00]).await.ok();
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
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    // 首字节 'H' 已被调用方消耗，补读方法行剩余部分
    let mut line = String::from("H");
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

    // 读完剩余头到空行
    loop {
        socket
            .read_exact(&mut b)
            .await
            .map_err(|e| e.to_string())?;
        if b[0] == b'\r' {
            // CRLF 空行判定：下一个是 \n 说明是空行
            let mut nxt = [0u8; 1];
            match socket.peek(&mut nxt).await {
                Ok(1) if nxt[0] == b'\n' => {
                    let _ = socket.read_exact(&mut nxt).await;
                    break;
                }
                _ => {}
            }
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
