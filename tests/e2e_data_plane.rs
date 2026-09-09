//! 数据面 e2e：真实跑起 `silverq serve`，用真 SOCKS5 / HTTP-CONNECT 客户端打流量。
//!
//! 全程**不依赖外网**：
//! - 目标服务是测试自己起的本地 TCP echo / UDP echo / HTTP 端点
//! - 节点用 `protocol: direct`（meow 的 DirectAdapter），所以"经代理"实际就是直连本地端点，
//!   但走的是完整数据面链路：inbound 协议解析 → EWMA 选择 → `dial_tcp`/`dial_udp` → 双向中继
//! - 探测 URL 用 `SILVERQ_PROBE_URL` 指到本地 HTTP 端点，调度间隔压到 1s
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

/// 起一个只回 204 的极简 HTTP 服务，给 silverq 当探测端点（替代 gstatic）。
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

/// 起一个 silverq daemon，进程退出时自动 kill。
struct Daemon {
    child: Child,
    socks: SocketAddr,
    _dir: tempdirlike::TempDir,
}

impl Daemon {
    /// 通过 ctl socket 发一条命令，返回 daemon 应答。
    fn ctl(&self, cmd: &str) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_silverq"))
            .args(cmd.split_whitespace())
            .env("SILVERQ_CTL_SOCK", self._dir.path().join("ctl.sock"))
            .output()
            .expect("ctl 调用失败");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 极简临时目录（不引 tempfile 依赖，测试用够了）。
mod tempdirlike {
    pub struct TempDir {
        path: std::path::PathBuf,
        /// 只有拥有者 drop 时才删目录
        owned: bool,
    }

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "silverq-e2e-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self {
                path: p,
                owned: true,
            }
        }
        pub fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl TempDir {
        /// 复用同一目录但**不接管删除责任**（用于跨"重启"共享状态文件）。
        ///
        /// 之前这里克隆出的句柄也会在 drop 时删目录，把第一个 daemon 写出的
        /// 状态文件连目录一起删掉，重启测试永远看不到存档。
        pub fn reuse(&self) -> Self {
            Self {
                path: self.path.clone(),
                owned: false,
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if self.owned {
                let _ = std::fs::remove_dir_all(&self.path);
            }
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

/// 起一个黑洞 TCP 服务：accept 后既不回数据也不关闭。
/// 模拟"TCP 连得上、握手看似成功、之后不响应"的节点 —— AEAD 协议上
/// `dial_tcp` 会立刻返回 Ok，只有首次响应超时能识别。
fn spawn_blackhole() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            match stream {
                Ok(s) => held.push(s), // 持住：不回、不关
                Err(_) => break,
            }
        }
    });
    addr
}

/// 启动 silverq serve，等到它把 selection 写出来（即第一轮测速完成）为止。
fn start_daemon(probe: SocketAddr) -> Daemon {
    start_daemon_with(probe, None)
}

/// `extra_nodes_before`: 插在 direct 节点**之前**的节点 YAML 片段，
/// 用来让某个坏节点排在候选队首，测 fallback。
fn start_daemon_with(probe: SocketAddr, extra_nodes_before: Option<&str>) -> Daemon {
    start_daemon_in(
        tempdirlike::TempDir::new("daemon"),
        probe,
        extra_nodes_before,
    )
}

/// 在指定目录起 daemon。目录复用 = 模拟重启（状态文件、配置都留着）。
fn start_daemon_in(
    dir: tempdirlike::TempDir,
    probe: SocketAddr,
    extra_nodes_before: Option<&str>,
) -> Daemon {
    let nodes_path = dir.path().join("nodes.yaml");
    let mut yaml = String::from("nodes:\n");
    if let Some(extra) = extra_nodes_before {
        yaml.push_str(extra);
    }
    yaml.push_str(
        "  - tag: \"direct-baseline\"\n    protocol: direct\n    server: \"-\"\n    port: 0\n",
    );
    std::fs::write(&nodes_path, yaml).unwrap();

    let socks_port = free_port();
    let socks: SocketAddr = format!("127.0.0.1:{socks_port}").parse().unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_silverq"))
        .arg("serve")
        .arg(&nodes_path)
        .env("SILVERQ_LISTEN", socks.to_string())
        .env("SILVERQ_CTL_SOCK", dir.path().join("ctl.sock"))
        .env("SILVERQ_SELECTOR_STORE", dir.path().join("selector.json"))
        .env("SILVERQ_STATE", dir.path().join("scores.json"))
        .env("SILVERQ_PROBE_URL", format!("http://{probe}/"))
        .env("SILVERQ_INTERVAL_SECS", "1")
        .env("SILVERQ_TIMEOUT_MS", "1500")
        .stdout(std::fs::File::create(dir.path().join("log")).unwrap())
        .stderr(std::process::Stdio::from(
            std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join("log"))
                .unwrap(),
        ))
        .spawn()
        .expect("启动 silverq 失败");

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
    s.write_all(b"hello-silverq").unwrap();

    let mut buf = [0u8; 10];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello-silverq", "TCP echo 应原样返回");
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

/// EWMA 必须把连不上的节点从队首挤走 —— 这是"自动挡"的核心保证。
///
/// 配置里死节点排第一，但一轮测速后它应该被扣分后移，数据面请求走活节点。
/// 这条比"测 fallback"更贴近真实价值：fallback 是兜底，EWMA 排序是主线。
#[test]
fn ewma_demotes_unreachable_node_from_head() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_tcp_echo();

    // 占一个端口再释放，得到一个几乎确定无人监听的端口
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let bad = format!(
        "  - tag: \"unreachable\"\n    protocol: shadowsocks\n    server: \"127.0.0.1\"\n    port: {dead_port}\n    shadowsocks:\n      password: \"0123456789abcdef\"\n      cipher: \"aes-128-gcm\"\n"
    );
    let d = start_daemon_with(probe, Some(&bad));

    // 死节点在配置里排第一，但测速后应被挤到 direct 之后
    let status = d.ctl("status");
    let head = status
        .split("selection=[")
        .nth(1)
        .and_then(|r| r.split(']').next())
        .unwrap_or("")
        .to_string();
    assert!(
        head.starts_with("\"direct-baseline\""),
        "EWMA 应把死节点挤出队首，实际 selection={head}"
    );

    // 且数据面确实可用
    let (mut s, _) = socks5_handshake(d.socks, 0x01, echo);
    s.write_all(b"ewma-works").unwrap();
    let mut buf = [0u8; 10];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ewma-works");
}

/// 钉住一个连不上的节点时，必须**快速失败**而不是挂住。
///
/// 钉住的语义是"只用这个节点"，所以不该 fallback（否则钉住就失去意义）。
/// 但也不能让请求悬着——实测应在毫秒级返回，而非拖到客户端超时。
#[test]
fn pinned_unreachable_node_fails_fast() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_tcp_echo();

    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let bad = format!(
        "  - tag: \"unreachable\"\n    protocol: shadowsocks\n    server: \"127.0.0.1\"\n    port: {dead_port}\n    shadowsocks:\n      password: \"0123456789abcdef\"\n      cipher: \"aes-128-gcm\"\n"
    );
    let d = start_daemon_with(probe, Some(&bad));

    let reply = d.ctl("select unreachable");
    assert!(reply.contains("pinned"), "钉住应成功，实际: {reply}");

    let started = Instant::now();
    let (mut s, _) = socks5_handshake(d.socks, 0x01, echo);
    s.write_all(b"should-fail").unwrap();
    let mut buf = [0u8; 11];
    let res = s.read_exact(&mut buf);

    assert!(res.is_err(), "钉住死节点不应 fallback 到活节点");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "必须快速失败，实际 {:?}",
        started.elapsed()
    );
}

/// 回归：钉住黑洞节点时必须在首次响应超时内失败，而不是挂到客户端超时。
///
/// AEAD 协议（shadowsocks）的 `dial_tcp` 不等服务端响应就返回 Ok，所以
/// dial 超时管不到黑洞节点。实测修复前会卡满 25s。
#[test]
fn pinned_blackhole_fails_within_first_response_timeout() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_tcp_echo();
    let bh = spawn_blackhole();

    let bad = format!(
        "  - tag: \"blackhole\"\n    protocol: shadowsocks\n    server: \"{}\"\n    port: {}\n    shadowsocks:\n      password: \"0123456789abcdef\"\n      cipher: \"aes-128-gcm\"\n",
        bh.ip(),
        bh.port()
    );
    let d = start_daemon_with(probe, Some(&bad));
    assert!(d.ctl("select blackhole").contains("pinned"));

    let started = Instant::now();
    let (mut s, _) = socks5_handshake(d.socks, 0x01, echo);
    s.write_all(b"into-the-void").unwrap();
    let mut buf = [0u8; 13];
    let res = s.read_exact(&mut buf);
    let elapsed = started.elapsed();

    assert!(res.is_err(), "黑洞节点不该返回数据");
    // SILVERQ_TIMEOUT_MS=1500 -> 首响超时 4x = 6s。上限收到 9s：
    // 松到 12s 时，客户端自己的 10s 读超时会先触发，超时被去掉也测不出来
    // （变异检验发现过这个漏洞）。
    assert!(
        elapsed < Duration::from_secs(9),
        "必须在首响超时(~6s)内失败，实际 {elapsed:?}"
    );
}

/// 回归：`select auto` 必须立即恢复 EWMA 排序，而不是等下一轮测速。
///
/// 早先 auto 只清 pinned 标志、不改 selection，于是"钉死节点 → auto"之后
/// selection 仍是那个死节点，请求继续全失败直到下一轮（间隔可能 30s+）。
#[test]
fn select_auto_restores_ewma_selection_immediately() {
    let probe = spawn_probe_endpoint();
    let echo = spawn_tcp_echo();
    let bh = spawn_blackhole();

    let bad = format!(
        "  - tag: \"blackhole\"\n    protocol: shadowsocks\n    server: \"{}\"\n    port: {}\n    shadowsocks:\n      password: \"0123456789abcdef\"\n      cipher: \"aes-128-gcm\"\n",
        bh.ip(),
        bh.port()
    );
    let d = start_daemon_with(probe, Some(&bad));

    d.ctl("select blackhole");
    let auto = d.ctl("select auto");
    assert!(
        auto.contains("direct-baseline"),
        "auto 应立即给出 EWMA 顺序，实际: {auto}"
    );

    // 解钉后数据面立刻可用，不必等下一轮测速
    let (mut s, _) = socks5_handshake(d.socks, 0x01, echo);
    s.write_all(b"after-auto").unwrap();
    let mut buf = [0u8; 10];
    s.read_exact(&mut buf).expect("解钉后应立即可用");
    assert_eq!(&buf, b"after-auto");
}

/// 回归：EWMA 分数必须跨进程重启保留，否则每次重启都要重新学习
/// （真实池一轮约 90s，期间只能盲选）。
#[test]
fn ewma_scores_survive_restart() {
    let probe = spawn_probe_endpoint();

    let dir = tempdirlike::TempDir::new("restart");
    let state = dir.path().join("scores.json");

    // 第一次启动：跑几轮测速，让分数落盘
    {
        let d = start_daemon_in(dir.reuse(), probe, None);
        // 等至少一轮结束（间隔 1s）后存盘
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            d.ctl("status").contains("direct-baseline"),
            "第一次启动应已选出节点"
        );
    } // daemon 在这里被 kill

    assert!(state.exists(), "重启前应已写出状态文件: {state:?}");
    let saved = std::fs::read_to_string(&state).unwrap();
    assert!(
        saved.contains("direct-baseline") && saved.contains("ewma"),
        "状态文件应含节点分数，实际: {saved}"
    );

    // 第二次启动（同目录）：日志应显示恢复了分数
    let d2 = start_daemon_in(dir.reuse(), probe, None);
    std::thread::sleep(Duration::from_millis(500));
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
    assert!(
        log.contains("从存档恢复"),
        "重启后应从存档恢复分数，日志: {log}"
    );
    drop(d2);
}
