//! SOCKS5 UDP ASSOCIATE 中继。
//!
//! 协议（RFC 1928 §7）：客户端在 TCP 上发 `CMD=0x03`，服务端回一个 UDP 中继地址；
//! 之后客户端把数据报发到该地址，每个包带头部：
//!
//! ```text
//! +-----+------+------+----------+----------+----------+
//! | RSV | FRAG | ATYP | DST.ADDR | DST.PORT |   DATA   |
//! +-----+------+------+----------+----------+----------+
//! |  2  |  1   |  1   | Variable |    2     | Variable |
//! +-----+------+------+----------+----------+----------+
//! ```
//!
//! 回程包同样带这个头部，DST 填**来源**地址。FRAG != 0 一律丢弃（不做重组，
//! 与主流实现一致）。
//!
//! # 会话模型
//!
//! meow 的 `ProxyPacketConn` 虽然是逐包寻址接口，但 `DirectAdapter::dial_udp`
//! 会按首个 metadata 的地址族绑 socket，且 meow 内部 NAT key 是 `(src, dst)` ——
//! 即**一个 conn 只应服务一个目标**。所以这里按 `(客户端地址, 目标地址)` 分会话，
//! 每个会话一个 `ProxyPacketConn` + 一个回程读取任务。
//!
//! TCP 控制连接是会话的存活信号：TCP 一断，整个 association 的所有会话回收
//! （RFC 1928 要求如此）。
#![cfg(feature = "meow")]

use crate::meow::Registry;
use meow_common::conn::ProxyPacketConn;
use meow_common::Metadata;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

/// 单个会话空闲多久后回收。DNS 查询很短，QUIC 连接会持续续期。
const SESSION_IDLE_SECS: u64 = 60;
/// 单个 UDP 数据报上限。QUIC 通常 <1500，留足余量。
const MAX_DATAGRAM: usize = 65_535;

/// 一条 association（对应一个 TCP 控制连接）持有的所有会话。
type Sessions = Arc<Mutex<HashMap<SessionKey, Arc<dyn ProxyPacketConn>>>>;

/// 会话键：(客户端来源, 目标)。与 meow 内部 NAT key 语义一致。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    client: SocketAddr,
    dst_host: String,
    dst_port: u16,
}

/// SOCKS5 UDP 头部解析结果。
pub struct UdpHeader {
    pub frag: u8,
    pub dst_host: String,
    pub dst_port: u16,
    /// DATA 在原 buffer 里的起始偏移
    pub payload_offset: usize,
}

/// 解析 SOCKS5 UDP 请求头。返回 None 表示包格式非法（应丢弃）。
pub fn parse_udp_header(buf: &[u8]) -> Option<UdpHeader> {
    if buf.len() < 5 {
        return None;
    }
    // buf[0..2] = RSV, 必须为 0（宽松处理：不校验，主流实现也不校验）
    let frag = buf[2];
    let atyp = buf[3];
    let (dst_host, dst_port, payload_offset) = match atyp {
        0x01 => {
            if buf.len() < 10 {
                return None;
            }
            let ip = std::net::Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
            let port = u16::from_be_bytes([buf[8], buf[9]]);
            (ip.to_string(), port, 10)
        }
        0x03 => {
            let len = buf[4] as usize;
            if buf.len() < 5 + len + 2 {
                return None;
            }
            let host = String::from_utf8_lossy(&buf[5..5 + len]).into_owned();
            let port = u16::from_be_bytes([buf[5 + len], buf[6 + len]]);
            (host, port, 7 + len)
        }
        0x04 => {
            if buf.len() < 22 {
                return None;
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[4..20]);
            let ip = std::net::Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([buf[20], buf[21]]);
            (ip.to_string(), port, 22)
        }
        _ => return None,
    };
    Some(UdpHeader {
        frag,
        dst_host,
        dst_port,
        payload_offset,
    })
}

/// 给回程数据加 SOCKS5 UDP 头部（DST 填来源地址）。
pub fn encode_udp_reply(src: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 22);
    out.extend_from_slice(&[0x00, 0x00, 0x00]); // RSV RSV FRAG
    match src {
        SocketAddr::V4(a) => {
            out.push(0x01);
            out.extend_from_slice(&a.ip().octets());
            out.extend_from_slice(&a.port().to_be_bytes());
        }
        SocketAddr::V6(a) => {
            out.push(0x04);
            out.extend_from_slice(&a.ip().octets());
            out.extend_from_slice(&a.port().to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// 建立 UDP 中继 socket，返回 (socket, 供客户端使用的地址)。
///
/// 绑在与 TCP 控制连接**同一个本地 IP** 上，端口由系统分配。
pub async fn bind_relay(
    control_local: SocketAddr,
) -> std::io::Result<(Arc<UdpSocket>, SocketAddr)> {
    let bind_ip = control_local.ip();
    let socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await?;
    let local = socket.local_addr()?;
    Ok((Arc::new(socket), local))
}

/// 运行 UDP 中继循环，直到 `shutdown` 被触发（TCP 控制连接断开）。
///
/// `selection` 决定用哪个节点：与 TCP 路径一致，按 best → 次优顺序尝试 `dial_udp`，
/// 跳过不支持 UDP 的 adapter（HTTP/SOCKS5 adapter 会返回 not-supported）。
pub async fn run_relay(
    relay: Arc<UdpSocket>,
    registry: Registry,
    selection: crate::inbound::SharedSelection,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let mut buf = vec![0u8; MAX_DATAGRAM];

    loop {
        let recv = tokio::select! {
            r = relay.recv_from(&mut buf) => r,
            _ = &mut shutdown => {
                tracing::debug!("udp associate: control connection closed, tearing down");
                return;
            }
        };

        let (n, client) = match recv {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("udp relay recv failed: {e}");
                continue;
            }
        };

        let Some(hdr) = parse_udp_header(&buf[..n]) else {
            tracing::debug!(%client, "udp: malformed header, dropped");
            continue;
        };
        if hdr.frag != 0 {
            // 不做分片重组：丢弃并记录（与主流实现一致）
            tracing::debug!(%client, frag = hdr.frag, "udp: fragmented packet dropped");
            continue;
        }
        let payload = &buf[hdr.payload_offset..n];

        let key = SessionKey {
            client,
            dst_host: hdr.dst_host.clone(),
            dst_port: hdr.dst_port,
        };

        // 取或建会话
        let conn = {
            let mut guard = sessions.lock().await;
            match guard.get(&key) {
                Some(c) => Arc::clone(c),
                None => {
                    let Some(conn) =
                        dial_udp_via_selection(&registry, &selection, &hdr.dst_host, hdr.dst_port)
                            .await
                    else {
                        tracing::warn!(
                            dst = %format!("{}:{}", hdr.dst_host, hdr.dst_port),
                            "udp: no udp-capable adapter in selection"
                        );
                        continue;
                    };
                    guard.insert(key.clone(), Arc::clone(&conn));

                    // 回程读取任务：把节点回包套上 SOCKS5 头发回客户端
                    spawn_reply_pump(
                        Arc::clone(&conn),
                        Arc::clone(&relay),
                        client,
                        Arc::clone(&sessions),
                        key.clone(),
                    );
                    conn
                }
            }
        };

        // 目标地址交给 meow 解析（可能是域名）
        let dst = match resolve_dst(&hdr.dst_host, hdr.dst_port) {
            Some(a) => a,
            None => {
                // 域名：meow 的 packet conn 自己会按 metadata 处理；
                // 这里用占位地址，实际寻址已在 dial 时通过 metadata 传入
                SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    hdr.dst_port,
                )
            }
        };

        if let Err(e) = conn.write_packet(payload, &dst).await {
            tracing::debug!(%client, "udp write failed: {e}");
            sessions.lock().await.remove(&key);
        }
    }
}

/// 按当前选择顺序尝试 `dial_udp`，跳过不支持 UDP 的节点。
async fn dial_udp_via_selection(
    registry: &Registry,
    selection: &crate::inbound::SharedSelection,
    dst_host: &str,
    dst_port: u16,
) -> Option<Arc<dyn ProxyPacketConn>> {
    let order = selection.read().await.clone();
    let candidates: Vec<_> = {
        let guard = registry.read();
        order
            .iter()
            .filter_map(|tag| guard.get(tag).cloned())
            .collect()
    };

    let metadata = Metadata {
        network: meow_common::Network::Udp,
        host: dst_host.into(),
        dst_port,
        ..Default::default()
    };

    for adapter in &candidates {
        if !adapter.support_udp() {
            continue;
        }
        match adapter.dial_udp(&metadata).await {
            Ok(c) => return Some(Arc::from(c)),
            Err(e) => tracing::debug!(tag = adapter.name(), "dial_udp failed: {e}"),
        }
    }
    None
}

/// 回程泵：读节点回包 → 套 SOCKS5 头 → 发回客户端。空闲超时后自行退出并清理会话。
fn spawn_reply_pump(
    conn: Arc<dyn ProxyPacketConn>,
    relay: Arc<UdpSocket>,
    client: SocketAddr,
    sessions: Sessions,
    key: SessionKey,
) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(SESSION_IDLE_SECS),
                conn.read_packet(&mut buf),
            )
            .await;

            match read {
                // 空闲超时：回收会话
                Err(_) => break,
                Ok(Err(e)) => {
                    tracing::debug!(%client, "udp read failed: {e}");
                    break;
                }
                Ok(Ok((n, src))) => {
                    let framed = encode_udp_reply(src, &buf[..n]);
                    if let Err(e) = relay.send_to(&framed, client).await {
                        tracing::debug!(%client, "udp reply send failed: {e}");
                        break;
                    }
                }
            }
        }
        sessions.lock().await.remove(&key);
    });
}

/// 目标是 IP 字面量时直接解析；域名返回 None（由 metadata 侧处理）。
fn resolve_dst(host: &str, port: u16) -> Option<SocketAddr> {
    host.parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ipv4_header() {
        // RSV RSV FRAG ATYP=1 1.1.1.1 :53 + payload
        let mut pkt = vec![0x00, 0x00, 0x00, 0x01, 1, 1, 1, 1];
        pkt.extend_from_slice(&53u16.to_be_bytes());
        pkt.extend_from_slice(b"query");

        let h = parse_udp_header(&pkt).unwrap();
        assert_eq!(h.dst_host, "1.1.1.1");
        assert_eq!(h.dst_port, 53);
        assert_eq!(h.frag, 0);
        assert_eq!(&pkt[h.payload_offset..], b"query");
    }

    #[test]
    fn parse_domain_header() {
        let host = b"example.com";
        let mut pkt = vec![0x00, 0x00, 0x00, 0x03, host.len() as u8];
        pkt.extend_from_slice(host);
        pkt.extend_from_slice(&443u16.to_be_bytes());
        pkt.extend_from_slice(b"data");

        let h = parse_udp_header(&pkt).unwrap();
        assert_eq!(h.dst_host, "example.com");
        assert_eq!(h.dst_port, 443);
        assert_eq!(&pkt[h.payload_offset..], b"data");
    }

    #[test]
    fn parse_ipv6_header() {
        let mut pkt = vec![0x00, 0x00, 0x00, 0x04];
        pkt.extend_from_slice(&[0u8; 15]);
        pkt.push(1); // ::1
        pkt.extend_from_slice(&53u16.to_be_bytes());
        pkt.extend_from_slice(b"x");

        let h = parse_udp_header(&pkt).unwrap();
        assert_eq!(h.dst_host, "::1");
        assert_eq!(h.dst_port, 53);
        assert_eq!(&pkt[h.payload_offset..], b"x");
    }

    #[test]
    fn reject_truncated_and_bad_atyp() {
        assert!(parse_udp_header(&[0x00, 0x00, 0x00]).is_none(), "太短");
        // ATYP=1 但地址被截断
        assert!(parse_udp_header(&[0x00, 0x00, 0x00, 0x01, 1, 1]).is_none());
        // 域名长度声明超出实际
        assert!(parse_udp_header(&[0x00, 0x00, 0x00, 0x03, 99, b'a']).is_none());
        // 未知 ATYP
        assert!(parse_udp_header(&[0x00, 0x00, 0x00, 0x09, 1, 2, 3, 4, 0, 53]).is_none());
    }

    #[test]
    fn reply_roundtrips_through_parser() {
        let src: SocketAddr = "8.8.8.8:53".parse().unwrap();
        let framed = encode_udp_reply(src, b"answer");

        // 回程头部格式与请求头部一致，可以用同一个解析器验证
        let h = parse_udp_header(&framed).unwrap();
        assert_eq!(h.dst_host, "8.8.8.8");
        assert_eq!(h.dst_port, 53);
        assert_eq!(&framed[h.payload_offset..], b"answer");
    }

    #[test]
    fn reply_encodes_ipv6_source() {
        let src: SocketAddr = "[::1]:443".parse().unwrap();
        let framed = encode_udp_reply(src, b"p");
        assert_eq!(framed[3], 0x04, "IPv6 来源必须用 ATYP=4");
        let h = parse_udp_header(&framed).unwrap();
        assert_eq!(h.dst_host, "::1");
        assert_eq!(h.dst_port, 443);
    }
}
