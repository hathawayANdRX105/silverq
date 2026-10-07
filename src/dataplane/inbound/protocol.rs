//! SOCKS5/HTTP-CONNECT 请求解析。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::runtime::{Cmd, Proto, Target};
/// SOCKS5 握手协商：VER 已被调用方读掉，这里读 NMETHODS + METHODS，
/// 应答 no-auth（0x00）。不支持认证——本地回环端口，鉴权交给绑定地址。
pub async fn socks5_greeting(
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
/// 支持 CMD=0x01 CONNECT 与 CMD=0x03 UDP ASSOCIATE；BIND(0x02) 不支持。
pub async fn read_socks5_target(
    socket: &mut TcpStream,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    let mut head = [0u8; 4];
    socket.read_exact(&mut head).await?;
    let cmd = match (head[0], head[1]) {
        (0x05, 0x01) => Cmd::Connect,
        (0x05, 0x03) => Cmd::UdpAssociate,
        _ => {
            // 回 0x07 command not supported（完整 10 字节）
            socket
                .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .ok();
            return Ok(Target {
                host: String::new(),
                port: 0,
                proto: Proto::Socks5,
                cmd: Cmd::Connect,
                replay: vec![],
            });
        }
    };

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
                cmd,
                replay: vec![],
            });
        }
    };

    // 16 字节既可能是真 IPv6，也可能是恰好 16 字符的域名（如
    // "www.bilibili.com"）。ATYP 已经标明客户端意图：0x04 = IPv6，
    // 0x03 = 域名。只有 ATYP=0x04 才该按地址解码，否则按域名原样保留——
    // 误转换会让 china 后缀表失配，国内站点被错误地送进代理候选链。
    let host = if head[3] == 0x04 && host_bytes.len() == 16 {
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&host_bytes);
        std::net::Ipv6Addr::from(addr).to_string()
    } else if head[3] == 0x01 && host_bytes.len() == 4 {
        host_bytes
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(".")
    } else {
        String::from_utf8_lossy(&host_bytes).into_owned()
    };
    Ok(Target {
        host,
        port,
        proto: Proto::Socks5,
        cmd,
        replay: vec![],
    })
}

/// 解析 HTTP 代理请求：CONNECT 或透明代理（GET/POST 绝对 URI）。
///
/// 读走的原始字节全部累积进 `replay`：透明代理模式下请求行+头部要原样
/// 回放给上游（上游看到的是一个完整的 HTTP 请求）；CONNECT 模式不需要回放，
/// 但读路径必须统一——头部必须精确读到空行为止，残留字节会被拷进隧道，
/// 污染客户端 TLS ClientHello。
pub async fn read_http_connect_target(
    socket: &mut TcpStream,
    first_byte: u8,
) -> Result<Target, Box<dyn std::error::Error + Send + Sync>> {
    // 首字节已被调用方消耗，补回后读完方法行
    let mut raw: Vec<u8> = vec![first_byte];
    let mut b = [0u8; 1];
    loop {
        socket.read_exact(&mut b).await.map_err(|e| e.to_string())?;
        raw.push(b[0]);
        if b[0] == b'\n' {
            break;
        }
        if raw.len() > 1024 {
            return Ok(bad_request(socket).await);
        }
    }

    // 请求行：METHOD SP REQUEST-TARGET SP HTTP-VERSION
    let line = String::from_utf8_lossy(&raw).into_owned();
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(bad_request(socket).await);
    }
    let is_connect = parts[0].eq_ignore_ascii_case("CONNECT");

    // 读完剩余头部，直到空行（CRLF CRLF），全部累积进 raw
    let mut header_bytes = 0usize;
    loop {
        let mut cur = Vec::with_capacity(64);
        loop {
            socket.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            header_bytes += 1;
            raw.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
            cur.push(b[0]);
            if header_bytes > 16 * 1024 {
                return Err("http: headers too large".into());
            }
        }
        // 空行（只剩 \r 或什么都没有）= 头部结束
        if cur.iter().all(|c| *c == b'\r') {
            break;
        }
        if header_bytes > 16 * 1024 {
            return Err("http: headers too large".into());
        }
    }

    if is_connect {
        // CONNECT host:port —— 目标是 authority，默认 443
        let (host, port) = match split_host_port(parts[1], 443) {
            Some(v) => v,
            None => return Ok(bad_request(socket).await),
        };
        return Ok(Target {
            host,
            port,
            proto: Proto::Http,
            cmd: Cmd::Connect,
            replay: vec![],
        });
    }

    // 透明代理：REQUEST-TARGET 必须是绝对 URI（`scheme://authority/path`）。
    // 浏览器/curl 配成 HTTP 代理时永远发绝对 URI；相对 URI（`GET /path`）
    // 说明对端以为在跟源站说话，不是代理客户端，直接 400。
    let uri = parts[1];
    let scheme_end = match uri.find("://") {
        Some(i) => i,
        None => return Ok(bad_request(socket).await),
    };
    let https = uri[..scheme_end].eq_ignore_ascii_case("https");
    let after = &uri[scheme_end + 3..];
    let authority = after.find(['/', '?', '#']).map_or(after, |i| &after[..i]);
    // `http://127.0.0.1:8090@evil.com/` 的主机是 evil.com，不是回环——
    // 若按首个 @ 切分，is_local_target 会被骗成直连本地，绕过代理策略。
    let host_part = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let default_port = if https { 443 } else { 80 };
    let (host, port) = match split_host_port(host_part, default_port) {
        Some(v) => v,
        None => return Ok(bad_request(socket).await),
    };
    if host.is_empty() {
        return Ok(bad_request(socket).await);
    }

    Ok(Target {
        host,
        port,
        proto: Proto::Http,
        cmd: Cmd::HttpProxy,
        replay: raw,
    })
}

/// 从 `host[:port]` 或绝对 URI 的 authority 段拆出主机与端口；
/// 无端口时取 `default_port`。支持 `[::1]:443` / `[::1]` 形式的 IPv6 字面量。
/// None = 端口不是合法 u16（或 IPv6 字面量缺右括号）。
fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // IPv6 字面量：[addr] 或 [addr]:port
        let (addr, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None => default_port,
        };
        Some((addr.to_string(), port))
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) => Some((h.to_string(), p.parse::<u16>().ok()?)),
            None => Some((s.to_string(), default_port)),
        }
    }
}

/// 回 `400 Bad Request` 并返回空 Target（调用方凭空 host 终止处理）。
async fn bad_request(socket: &mut TcpStream) -> Target {
    socket
        .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
        .await
        .ok();
    Target {
        host: String::new(),
        port: 0,
        proto: Proto::Http,
        cmd: Cmd::Connect,
        replay: vec![],
    }
}
