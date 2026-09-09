//! 数据面 e2e：真实跑起 `lift serve`，用真 SOCKS5 / HTTP-CONNECT 客户端打流量。
//!
//! 全程**不依赖外网**：
//! - 目标服务是测试自己起的本地 TCP echo / UDP echo / HTTP 端点
//! - 节点用 `protocol: direct`（meow 的 DirectAdapter），所以"经代理"实际就是直连本地端点，
//!   但走的是完整数据面链路：inbound 协议解析 → EWMA 选择 → `dial_tcp`/`dial_udp` → 双向中继
//! - 探测 URL 用 `LIFT_PROBE_URL` 指到本地 HTTP 端点，调度间隔压到 1s
//!
//! 需要 `--features meow`（数据面在该 feature 下）。没有该 feature 时整个文件不编译内容。
#![cfg(feature = "meow")]

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// 起一个 TCP echo 服务，返回其地址。
fn spawn_tcp_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 || s.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// 起一个 UDP echo 服务，返回其地址。
fn spawn_udp_echo() -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while let Ok((n, src)) = socket.recv_from(&mut buf) {
            let _ = socket.send_to(&buf[..n], src);
        }
    });
    addr
}

/// 起一个只回 204 的极简 HTTP 服务，给 lift 当探测端点（替代 gstatic）。
fn spawn_probe_endpoint() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                // 读掉请求头（到空行），然后回 204
                let mut buf = [0u8; 1];
                let mut consecutive_newlines = 0;
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        return;
                    }
                    match buf[0] {
                        b'\n' => {
                            consecutive_newlines += 1;
                            if consecutive_newlines == 2 {
                                break;
                            }
                        }
                        b'\r' => {}
                        _ => consecutive_newlines = 0,
                    }
                }
                let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
                let _ = s.flush();
            });
        }
    });
    addr
}

/// 起一个 lift daemon，进程退出时自动 kill。
struct Daemon {
    child: Child,
    socks: SocketAddr,
    _dir: tempdirlike::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 极简临时目录（不引 tempfile 依赖，测试用够了）。
mod tempdirlike {
    pub struct TempDir(pub std::path::PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "lift-e2e-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 启动 lift serve，等到它把 selection 写出来（即第一轮测速完成）为止。
fn start_daemon(probe: SocketAddr) -> Daemon {
    let dir = tempdirlike::TempDir::new("daemon");
    let nodes_path = dir.path().join("nodes.yaml");
    std::fs::write(
        &nodes_path,
        "nodes:\n  - tag: \"direct-baseline\"\n    protocol: direct\n    server: \"-\"\n    port: 0\n",
    )
    .unwrap();

    let socks_port = free_port();
    let socks: SocketAddr = format!("127.0.0.1:{socks_port}").parse().unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_lift"))
        .arg("serve")
        .arg(&nodes_path)
        .env("LIFT_LISTEN", socks.to_string())
        .env("LIFT_CTL_SOCK", dir.path().join("ctl.sock"))
        .env("LIFT_SELECTOR_STORE", dir.path().join("selector.json"))
        .env("LIFT_PROBE_URL", format!("http://{probe}/"))
        .env("LIFT_INTERVAL_SECS", "1")
        .env("LIFT_TIMEOUT_MS", "1500")
        .spawn()
        .expect("启动 lift 失败");

    let daemon = Daemon {
        child,
        socks,
        _dir: dir,
    };

    // 等 inbound 端口可连（说明进程起来了）
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&daemon.socks, Duration::from_millis(200)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 再等一轮调度，确保 selection 非空（否则 dial 时没有候选）
    std::thread::sleep(Duration::from_millis(2500));
    daemon
}

/// 完成 SOCKS5 握手，返回已建立的流。`cmd`: 0x01 CONNECT / 0x03 UDP ASSOCIATE。
/// 返回 (流, 服务端应答里的 BND 地址)。
fn socks5_handshake(proxy: SocketAddr, cmd: u8, dst: SocketAddr) -> (TcpStream, SocketAddr) {
    let mut s = TcpStream::connect(proxy).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

    // greeting: VER=5, NMETHODS=1, no-auth
    s.write_all(&[0x05, 0x01, 0x00]).unwrap();
    let mut greet = [0u8; 2];
    s.read_exact(&mut greet).unwrap();
    assert_eq!(greet, [0x05, 0x00], "greeting 应回 no-auth");

    // request: VER CMD RSV ATYP=1 IPv4 PORT
    let SocketAddr::V4(v4) = dst else {
        panic!("测试只用 IPv4 目标")
    };
    let mut req = vec![0x05, cmd, 0x00, 0x01];
    req.extend_from_slice(&v4.ip().octets());
    req.extend_from_slice(&v4.port().to_be_bytes());
    s.write_all(&req).unwrap();

    // reply: VER REP RSV ATYP BND.ADDR(4) BND.PORT(2) —— 必须是完整 10 字节
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], 0x05, "应答 VER");
    assert_eq!(reply[1], 0x00, "应答 REP 应为 success，实际 {}", reply[1]);
    assert_eq!(reply[3], 0x01, "测试期望 IPv4 BND");

    let bnd = SocketAddr::from((
        [reply[4], reply[5], reply[6], reply[7]],
        u16::from_be_bytes([reply[8], reply[9]]),
    ));
    (s, bnd)
}

#[test]
fn socks5_tcp_connect_relays_traffic() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_tcp_echo();
    let d = start_daemon(probe);

    let (mut s, _) = socks5_handshake(d.socks, 0x01, echo);
    s.write_all(b"hello-lift").unwrap();

    let mut buf = [0u8; 10];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello-lift", "TCP echo 应原样返回");
}

#[test]
fn socks5_udp_associate_relays_datagrams() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_udp_echo();
    let d = start_daemon(probe);

    // UDP ASSOCIATE：控制连接必须保持打开，否则 association 被回收
    let (_control, relay) = socks5_handshake(
        d.socks,
        0x03,
        "0.0.0.0:0".parse().unwrap(), // 客户端不知道自己的源地址，按 RFC 填全零
    );
    assert_ne!(relay.port(), 0, "服务端必须返回可用的中继端口");

    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // 封 SOCKS5 UDP 头：RSV RSV FRAG ATYP=1 dst dst_port + payload
    let SocketAddr::V4(echo_v4) = echo else {
        unreachable!()
    };
    let mut pkt = vec![0x00, 0x00, 0x00, 0x01];
    pkt.extend_from_slice(&echo_v4.ip().octets());
    pkt.extend_from_slice(&echo_v4.port().to_be_bytes());
    pkt.extend_from_slice(b"ping-udp");

    client.send_to(&pkt, relay).unwrap();

    let mut buf = [0u8; 1024];
    let (n, from) = client.recv_from(&mut buf).expect("应收到 UDP 回程包");
    assert_eq!(from, relay, "回程包应来自中继地址");

    // 回程同样带 SOCKS5 头，DST 填来源（即 echo 服务）
    assert!(n > 10, "回程包应含头部 + payload，实际 {n} 字节");
    assert_eq!(buf[2], 0x00, "FRAG 应为 0");
    assert_eq!(buf[3], 0x01, "来源是 IPv4，ATYP 应为 1");
    let src_port = u16::from_be_bytes([buf[8], buf[9]]);
    assert_eq!(src_port, echo_v4.port(), "回程头部的来源端口应是 echo 服务");
    assert_eq!(&buf[10..n], b"ping-udp", "UDP echo 应原样返回");
}

#[test]
fn socks5_udp_rejects_fragmented_packet() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_udp_echo();
    let d = start_daemon(probe);

    let (_control, relay) = socks5_handshake(d.socks, 0x03, "0.0.0.0:0".parse().unwrap());

    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(1200)))
        .unwrap();

    let SocketAddr::V4(echo_v4) = echo else {
        unreachable!()
    };
    // FRAG=1 → 必须被丢弃（不做重组）
    let mut pkt = vec![0x00, 0x00, 0x01, 0x01];
    pkt.extend_from_slice(&echo_v4.ip().octets());
    pkt.extend_from_slice(&echo_v4.port().to_be_bytes());
    pkt.extend_from_slice(b"frag");
    client.send_to(&pkt, relay).unwrap();

    let mut buf = [0u8; 1024];
    assert!(
        client.recv_from(&mut buf).is_err(),
        "分片包必须被丢弃，不应有回程"
    );
}

#[test]
fn http_connect_relays_traffic() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_tcp_echo();
    let d = start_daemon(probe);

    let mut s = TcpStream::connect(d.socks).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!("CONNECT {echo} HTTP/1.1\r\nHost: {echo}\r\nProxy-Connection: Keep-Alive\r\n\r\n")
            .as_bytes(),
    )
    .unwrap();

    // 读到 CONNECT 应答的空行为止
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    loop {
        s.read_exact(&mut b).unwrap();
        head.push(b[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        assert!(head.len() < 4096, "应答头过长");
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.contains("200"), "CONNECT 应回 200，实际: {head}");

    // 隧道内跑 echo，验证头部没有残留字节污染隧道
    s.write_all(b"tunnel-clean").unwrap();
    let mut buf = [0u8; 12];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"tunnel-clean", "隧道数据应原样返回（头部无残留）");

    s.shutdown(Shutdown::Both).ok();
}

#[test]
fn socks5_rejects_bind_command() {
    let probe = spawn_probe_endpoint();
    let d = start_daemon(probe);

    let mut s = TcpStream::connect(d.socks).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(&[0x05, 0x01, 0x00]).unwrap();
    let mut greet = [0u8; 2];
    s.read_exact(&mut greet).unwrap();

    // CMD=0x02 BIND 不支持
    s.write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
        .unwrap();
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[1], 0x07, "BIND 应回 0x07 command not supported");
}
