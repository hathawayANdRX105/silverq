#![cfg(feature = "meow")]

//! inbound 数据面单元测试：超时派生、SOCKS5/HTTP-CONNECT 目标解析、候选挑选。
use silverq::dataplane::inbound::*;
use silverq::proxy::meow::Registry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;

/// dial / 首响超时必须由**配置传入的** timeout_ms 派生。
///
/// 回归一个真 bug：这两处曾读 `config::timeout_ms()`，那个函数只看
/// SILVERQ_TIMEOUT_MS 环境变量，把 silverq.toml 里的值悄悄丢掉 ——
/// 测速用 2500 而数据面按默认 2000 派生，配置在数据面这条路径上是死的。
/// 现在 `config::timeout_ms()` 已删除，这条测试锁住派生关系。
#[test]
fn dial_timeouts_derive_from_configured_timeout() {
    let t = DialTuning {
        timeout_ms: 2500,
        fallback_attempts: 3,
    };
    assert_eq!(
        t.dial(),
        std::time::Duration::from_millis(2500),
        "dial = 1x"
    );
    assert_eq!(
        t.first_response(),
        std::time::Duration::from_millis(10_000),
        "首响 = 4x"
    );

    // 换个值必须跟着变（若还硬编码 config 默认 2000，这里会失败）
    let t2 = DialTuning {
        timeout_ms: 4000,
        fallback_attempts: 1,
    };
    assert_eq!(t2.dial(), std::time::Duration::from_millis(4000));

    assert_eq!(
        t2.first_response(),
        std::time::Duration::from_millis(16_000)
    );
}

/// 建一对本地连接：返回 (客户端侧, 服务端侧)。
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr);
    let server = listener.accept();
    let (client, server) = tokio::join!(client, server);
    (client.unwrap(), server.unwrap().0)
}

#[test]
fn pick_candidates_truncates_to_fallback_limit() {
    use meow_common::adapter::ProxyAdapter;
    let reg: Registry = std::sync::Arc::new(parking_lot::RwLock::new(
        ["a", "b", "c", "d"]
            .iter()
            .map(|t| {
                (
                    t.to_string(),
                    std::sync::Arc::new(meow_proxy::DirectAdapter::new())
                        as std::sync::Arc<dyn ProxyAdapter>,
                )
            })
            .collect(),
    ));
    let map = reg.read();
    let order = vec![
        "a".into(),
        "b".into(),
        "c".into(),
        "d".into(),
        "missing".into(),
    ];
    // 上限 2：只取前两个，且不存在的 tag 被跳过后**不占名额**。
    // 返回 (tag, adapter)：tag 即 order 里的节点名（HP 归因落点），
    // adapter 的 name() 是协议内置名（DIRECT 之类），按 tag 断言。
    let got = pick_candidates(&order, &map, 2);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, "a");
    assert_eq!(got[0].1.name(), map["a"].name());
    assert_eq!(got[1].0, "b");
    assert_eq!(got[1].1.name(), map["b"].name());

    // 上限 1：只有队首
    assert_eq!(pick_candidates(&order, &map, 1).len(), 1);

    // 上限大：全部存在节点（missing 跳过）
    assert_eq!(pick_candidates(&order, &map, 10).len(), 4);

    // 上限 0：至少保底 1 个（max(1)），完全不放行会让请求必死
    assert_eq!(pick_candidates(&order, &map, 0).len(), 1);
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

    assert!(target.replay.is_empty(), "CONNECT 不回放任何字节");
    // 关键断言：socket 里剩下的必须正好是隧道数据，没有残留头部字节
    let mut rest = [0u8; 9];
    server.read_exact(&mut rest).await.unwrap();
    assert_eq!(&rest, b"\x16\x03\x01TUNNEL", "头部残留会污染隧道数据");
}

/// 透明 HTTP 代理：绝对 URI 的 authority 才是转发目标，路径留在回放里。
/// `curl -x http://127.0.0.1:17321 http://example.com:8080/x` 走这条路径。
#[tokio::test]
async fn http_proxy_absolute_uri_target() {
    let (mut client, mut server) = pair().await;
    client
        .write_all(
            b"ET http://example.com:8080/path?a=1 HTTP/1.1\r\nHost: example.com:8080\r\n\r\n",
        )
        .await
        .unwrap();

    let target = read_http_connect_target(&mut server, b'G').await.unwrap();
    assert_eq!(target.host, "example.com");
    assert_eq!(target.port, 8080);
    // 回放 = 请求行 + 全部头部（精确到空行），路径留在回放里带给上游
    assert_eq!(
        target.replay,
        b"GET http://example.com:8080/path?a=1 HTTP/1.1\r\nHost: example.com:8080\r\n\r\n",
        "回放必须精确等于读走的字节，多一个少一个都会破坏上游收到的请求"
    );
}

/// 无端口的绝对 URI：http 默认 80，https 默认 443。
#[tokio::test]
async fn http_proxy_default_ports() {
    let cases: Vec<(&[u8], u8, &str, u16)> = vec![
        (
            b"ET http://example.com/path HTTP/1.1\r\n\r\n" as &[u8],
            b'G',
            "example.com",
            80,
        ),
        (
            b"ET https://example.com/path HTTP/1.1\r\n\r\n" as &[u8],
            b'G',
            "example.com",
            443,
        ),
    ];
    for (req, first, expect_host, expect_port) in cases {
        let (mut client, mut server) = pair().await;
        client.write_all(req).await.unwrap();
        let target = read_http_connect_target(&mut server, first).await.unwrap();
        assert_eq!(target.host, expect_host);
        assert_eq!(target.port, expect_port);
    }
}

/// 回归一个真实安全漏洞形态：`http://127.0.0.1:8090@evil.com/` 的主机是
/// evil.com（userinfo 在最后一个 @ 之后）。按首个 @ 切分会把 host 当成
/// 127.0.0.1:8090，is_local_target 判为回环直连——攻击者借"代理"把请求
/// 伪装成本地目标，绕过代理策略直达本机服务。
#[tokio::test]
async fn http_proxy_userinfo_after_last_at_is_not_the_host() {
    let (mut client, mut server) = pair().await;
    client
        .write_all(b"ET http://127.0.0.1:8090@evil.com/ HTTP/1.1\r\n\r\n")
        .await
        .unwrap();

    let target = read_http_connect_target(&mut server, b'G').await.unwrap();
    assert_eq!(target.host, "evil.com", "userinfo 不是主机");
    assert!(!is_local_target(&target.host), "不能被判成回环直连");
}

/// 回放之后必须只剩正文：POST 的 Content-Length 正文留在 socket 里，
/// 由后续中继带走，头部一个字节都不能多读。
#[tokio::test]
async fn http_proxy_replay_stops_at_blank_line() {
    let (mut client, mut server) = pair().await;
    client
        .write_all(b"OST http://example.com/submit HTTP/1.1\r\nContent-Length: 5\r\n\r\nBODY!")
        .await
        .unwrap();

    let target = read_http_connect_target(&mut server, b'P').await.unwrap();
    assert_eq!(target.host, "example.com");
    assert!(target.replay.ends_with(b"\r\n\r\n"));
    assert!(!target.replay.contains(&b'B'));

    let mut rest = [0u8; 5];
    server.read_exact(&mut rest).await.unwrap();
    assert_eq!(&rest, b"BODY!", "正文必须完整留在 socket 里");
}

/// 相对 URI（`GET /path HTTP/1.1`）：对端不是代理客户端，400 拒绝。
/// 不做 Host 头兜底——配置成代理的客户端永远发绝对 URI，YAGNI。
#[tokio::test]
async fn http_relative_uri_rejected_with_400() {
    let (mut client, mut server) = pair().await;
    client
        .write_all(b"ET /path HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();

    let target = read_http_connect_target(&mut server, b'G').await.unwrap();
    assert!(target.host.is_empty(), "相对 URI 不应产生转发目标");

    let mut buf = vec![0u8; 32];
    let n = client.read(&mut buf).await.unwrap();
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("400"),
        "应回 400 而不是 405（405 会让客户端误以为方法不允许而重试）"
    );
}

// ── 首字节早期记账 + 实际流量 HP 归因（loopback 确定性场景）─────────────────

use silverq::proxy::route::{Decision, Route, RouteCache};
use silverq::scheduler::node::{Node, HP_INITIAL};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 有界等待 `cond` 为真（防 CI 挂死：不成立则超时失败并给出当时值）。
async fn wait_for(cond: impl Fn() -> bool, what: &str) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时：{what}");
}

/// 早期路线与 HP 记账：relay 在代理首字节写回客户端时更新缓存与健康度，
/// 长连接尚未关闭时，同域名下一连接已经有可复用的路线。
#[tokio::test]
async fn relay_records_route_and_hp_on_first_byte_while_connection_open() {
    // 上游（relay 的 conn 端）：accept 后立即回首字节，然后挂住（长会话）
    let upstream_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_l.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let (up, _) = upstream_l.accept().await.unwrap();
        let mut up = up;
        up.write_all(b"OK").await.unwrap();
        stop_rx.await.ok();
        drop(up);
    });

    // 客户端侧：pair() 两端做 relay 的 socket（quiet 端保持打开）
    let (client, quiet_peer) = pair().await;
    let conn: Box<dyn meow_common::conn::ProxyConn> =
        Box::new(TcpStream::connect(upstream_addr).await.unwrap());

    let routes = Arc::new(RouteCache::new());
    let pool: SharedPool = Arc::new(tokio::sync::RwLock::new(vec![Node::new(
        "fast",
        "127.0.0.1",
        443,
    )]));
    let relay_routes = Arc::clone(&routes);
    let relay_pool = Arc::clone(&pool);
    let host = "cache-early-recorder.test";
    let handle = tokio::spawn(async move {
        let recorder = Some(RouteRecorder::new(&relay_routes, host, Route::Proxy));
        relay(
            client,
            conn,
            Duration::from_secs(5),
            vec![],
            Instant::now(),
            recorder,
            Some((&relay_pool, "fast")),
        )
        .await
    });

    // 上游已回首字节 → 缓存条目此刻就该存在（连接仍开着、relay 未收尾）
    wait_for(
        || matches!(routes.decide(host), Decision::Proxy),
        "代理路线应在首字节落地时出现",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pool.read().await[0].hp == HP_INITIAL + 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("代理响应时应给节点加 HP，不等长连接关闭");
    assert!(!handle.is_finished(), "连接仍开启时路线与 HP 应已入账");

    // 收尾：停掉上游、关掉客户端对端，relay 任务收敛（结果形态不限：
    // 客户端早退的拆隧语义下 Neutral/晚死 Stream 都合法，本测试只关心
    // 「早期已记账」）
    stop_tx.send(()).ok();
    drop(quiet_peer);
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

/// 竞速版早期记账：代理侧赢得首字节时，路线缓存条目与 HP 加分都发生在
/// **首字节落地当下**（连接仍开着、双向拷贝没收尾）。旧实现下 race_relay
/// 只在 copy 结束后才返回 Route——长会话期间缓存没有条目，后续每个
/// 连接都会重新进竞速（反复烧双探预算）。
#[tokio::test]
async fn race_records_route_and_hp_on_first_byte_while_connection_open() {
    // 直连侧：accept 即持有、永不出数据（silent winner-loser）
    let silent_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_addr = silent_l.local_addr().unwrap();
    tokio::spawn(async move {
        let (s, _) = silent_l.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    });
    // 代理侧（"fast"）：100ms 后拨通，连上立刻回首字节，然后挂住
    let win_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let win_addr = win_l.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = win_l.accept().await.unwrap();
        s.write_all(b"WIN").await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    let dial = |addr: std::net::SocketAddr, delay: Duration| async move {
        tokio::time::sleep(delay).await;
        match tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await {
            Ok(Ok(s)) => Some(Box::new(s) as Box<dyn meow_common::conn::ProxyConn>),
            _ => None,
        }
    };
    let waves: Vec<Vec<SideSpec<'static>>> = vec![vec![
        SideSpec {
            route: Route::Direct,
            tag: None,
            fut: Box::pin(dial(silent_addr, Duration::ZERO)),
        },
        SideSpec {
            route: Route::Proxy,
            tag: Some("fast".into()),
            fut: Box::pin(dial(win_addr, Duration::from_millis(100))),
        },
    ]];

    // race 的 socket 侧（客户端视角）
    let (mut client, sock) = pair().await;
    let routes = Arc::new(RouteCache::new());
    let pool: SharedPool = Arc::new(tokio::sync::RwLock::new(vec![Node::new(
        "fast", "10.9.8.7", 1080,
    )]));
    let race_routes = Arc::clone(&routes);
    let race_pool = Arc::clone(&pool);
    let host = "race-early-recorder.test";
    let handle = tokio::spawn(async move {
        race_relay(
            sock,
            &[],
            Duration::from_secs(5),
            waves,
            false,
            Instant::now(),
            RaceCtx {
                routes: &race_routes,
                host,
                pool: &race_pool,
            },
        )
        .await
    });

    // 胜者首字节到达客户端
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("应在 2s 内拿到胜者首字节")
        .unwrap();
    assert_eq!(&buf[..n], b"WIN");

    // 此刻 race 任务仍在双向拷贝（长会话）：路线条目与 HP 都该已落地
    assert!(
        matches!(routes.decide(host), Decision::Proxy),
        "赢的代理路线应在首字节落地时入账（连接仍开着），而不是等收尾"
    );
    {
        let guard = pool.read().await;
        assert_eq!(
            guard[0].hp,
            HP_INITIAL + 2,
            "代理侧胜出应在首字节落地时即 +HP"
        );
    }
    assert!(
        !handle.is_finished(),
        "记账发生时竞速必须未收尾（连接还开着）"
    );

    // 收尾：关掉客户端 → 竞速收敛（客户端早退下 Won 或晚死 Stream 都合法）
    drop(client);
    match tokio::time::timeout(Duration::from_secs(5), handle).await {
        Ok(Ok(RaceOutcome::Won)) => {}
        Ok(Ok(RaceOutcome::DiedAfterWin(_))) => {}
        other => panic!("收尾应正常完成，实际 {other:?}"),
    }
}

/// 实际流量 HP 记账（纯函数，确定性）：成功 +2 / 失败 -15 / 未知 tag 不动；
/// 冷却排序：刚失败的节点后置，冷却窗外恢复 EWMA 顺序。
#[test]
fn runtime_hp_outcomes_and_reorder() {
    let mut nodes = vec![
        Node::new("fast", "1.1.1.1", 1080),
        Node::new("backup", "2.2.2.2", 1080),
    ];

    // 成功：+2（50 → 52），且清瞬态失败时刻
    apply_runtime_outcome(&mut nodes, "fast", true);
    assert_eq!(nodes[0].hp, HP_INITIAL + 2);
    assert!(nodes[0].last_runtime_failure.is_none());

    // 失败：-15（52 → 37），并打上瞬态失败时刻（冷却排序的依据）
    apply_runtime_outcome(&mut nodes, "fast", false);
    assert_eq!(nodes[0].hp, HP_INITIAL + 2 - 15);
    assert!(nodes[0].last_runtime_failure.is_some());

    // 未知 tag：静默跳过（节点刚被摘除等）
    let before = nodes[1].hp;
    apply_runtime_outcome(&mut nodes, "gone", false);
    assert_eq!(nodes[1].hp, before);

    // 饱和下界 0
    let mut one = vec![Node::new("x", "3.3.3.3", 1080)];
    for _ in 0..5 {
        apply_runtime_outcome(&mut one, "x", false);
    }
    assert_eq!(one[0].hp, 0);

    // 冷却排序：fast 刚失败（上面的时刻），30s 窗内 → 后置
    let order = vec!["fast".to_string(), "backup".to_string()];
    let reordered = reorder_recent_failures(order.clone(), &nodes, RECENT_FAILURE_COOLDOWN);
    assert_eq!(reordered, vec!["backup", "fast"]);
    // 冷却窗为 0：没有任何节点在窗内 → 保持 EWMA 原序
    let untouched = reorder_recent_failures(order.clone(), &nodes, Duration::ZERO);
    assert_eq!(untouched, order);
    // 未失败的节点不被动
    let order2 = vec!["backup".to_string(), "fast".to_string()];
    let mut nodes2 = vec![Node::new("fast", "1.1.1.1", 1080)];
    apply_runtime_outcome(&mut nodes2, "fast", true); // 成功清时刻
    assert_eq!(
        reorder_recent_failures(order2.clone(), &nodes2, RECENT_FAILURE_COOLDOWN),
        order2
    );
}

// -- 竞速循环（race_relay）行为测试：从 src/dataplane/inbound.rs 迁入 --

/// 假客户端：连上 race_relay 侧的 socket，返回 (客户端句柄, 服务端句柄)。
async fn client_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let client = TcpStream::connect(addr).await.unwrap();
    let (sock, _) = l.accept().await.unwrap();
    (client, sock)
}

/// TCP 秒连但永不出数据的一侧必须输掉竞速（#23 盲区回归）：直连侧先
/// 连通且全程静默，代理侧晚 100ms 才拨通但立刻回数据 → 代理侧首字节判胜。
/// 旧 TCP 竞速在这组时序下会选静默的直连侧，然后空等到超时。
#[tokio::test]
async fn race_winner_is_first_byte_not_first_connect() {
    // 直连侧：accept 即持有，永不出数据
    let silent_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_addr = silent_l.local_addr().unwrap();
    tokio::spawn(async move {
        let (s, _) = silent_l.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    });
    // 代理侧：100ms 后才拨通，连上立刻回首字节
    let resp_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let resp_addr = resp_l.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = resp_l.accept().await.unwrap();
        s.write_all(b"FIRST-BYTE").await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    let dial = |addr: std::net::SocketAddr, delay: Duration| async move {
        tokio::time::sleep(delay).await;
        match tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await {
            Ok(Ok(s)) => Some(Box::new(s) as Box<dyn meow_common::conn::ProxyConn>),
            _ => None,
        }
    };
    let waves: Vec<Vec<SideSpec<'static>>> = vec![vec![
        SideSpec {
            route: Route::Direct,
            tag: None,
            fut: Box::pin(dial(silent_addr, Duration::ZERO)),
        },
        SideSpec {
            route: Route::Proxy,
            tag: None,
            fut: Box::pin(dial(resp_addr, Duration::from_millis(100))),
        },
    ]];

    let routes = Arc::new(RouteCache::new());
    let pool: SharedPool = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let race_routes = Arc::clone(&routes);
    let race_pool = Arc::clone(&pool);

    let (mut client, sock) = client_pair().await;
    let handle = tokio::spawn(async move {
        race_relay(
            sock,
            &[],
            Duration::from_secs(5),
            waves,
            false,
            Instant::now(),
            RaceCtx {
                routes: &race_routes,
                host: "race-test.host",
                pool: &race_pool,
            },
        )
        .await
    });

    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("客户端应在 2s 内拿到首字节")
        .unwrap();
    assert_eq!(&buf[..n], b"FIRST-BYTE", "应收到代理侧首字节");
    drop(client); // 客户端关闭 → 胜者双向拷贝收尾 → race_relay 返回

    match handle.await.unwrap() {
        RaceOutcome::Won | RaceOutcome::DiedAfterWin(_) => {}
        other => panic!("应判胜，实际 {other:?}"),
    }
}

/// 双败后第二波并行兜底（#23）：波次 1 的直连与首选都拨号即败，第 2 波
/// 的候选救回请求——而不是串行逐个试到第三轮。
#[tokio::test]
async fn race_second_wave_rescues_after_double_fail() {
    // 无监听的端口：loopback connect 立即 ECONNREFUSED
    let dead_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead_l.local_addr().unwrap();
    drop(dead_l);

    let resp_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let resp_addr = resp_l.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = resp_l.accept().await.unwrap();
        s.write_all(b"RESCUED").await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    let dial = |addr: std::net::SocketAddr| async move {
        match tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await {
            Ok(Ok(s)) => Some(Box::new(s) as Box<dyn meow_common::conn::ProxyConn>),
            _ => None,
        }
    };
    let waves: Vec<Vec<SideSpec<'static>>> = vec![
        vec![
            SideSpec {
                route: Route::Direct,
                tag: None,
                fut: Box::pin(dial(dead_addr)),
            },
            SideSpec {
                route: Route::Proxy,
                tag: None,
                fut: Box::pin(dial(dead_addr)),
            },
        ],
        vec![SideSpec {
            route: Route::Proxy,
            tag: None,
            fut: Box::pin(dial(resp_addr)),
        }],
    ];

    let routes = Arc::new(RouteCache::new());
    let pool: SharedPool = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let race_routes = Arc::clone(&routes);
    let race_pool = Arc::clone(&pool);

    let (mut client, sock) = client_pair().await;
    let handle = tokio::spawn(async move {
        race_relay(
            sock,
            &[],
            Duration::from_secs(5),
            waves,
            false,
            Instant::now(),
            RaceCtx {
                routes: &race_routes,
                host: "race-test2.host",
                pool: &race_pool,
            },
        )
        .await
    });

    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("第二波应救回客户端")
        .unwrap();
    assert_eq!(&buf[..n], b"RESCUED");
    drop(client);

    match handle.await.unwrap() {
        RaceOutcome::Won | RaceOutcome::DiedAfterWin(_) => {}
        other => panic!("第二波应救回，实际 {other:?}"),
    }
}

/// POST 守卫（#23）：非幂等方法的透明 HTTP 不允许双发，否则源站收到
/// 两遍副作用请求；CONNECT/SOCKS（空 replay）与安全方法放行。
#[test]
fn only_safe_requests_are_race_eligible() {
    assert!(replay_race_safe(b""));
    assert!(replay_race_safe(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"));
    assert!(replay_race_safe(b"HEAD / HTTP/1.1\r\nHost: a\r\n\r\n"));
    assert!(replay_race_safe(b"OPTIONS / HTTP/1.1\r\nHost: a\r\n\r\n"));
    assert!(!replay_race_safe(
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhello"
    ));
    assert!(!replay_race_safe(b"PUT /x HTTP/1.1\r\nHost: a\r\n\r\n"));
    assert!(!replay_race_safe(b"DELETE /x HTTP/1.1\r\nHost: a\r\n\r\n"));
}

#[test]
fn local_targets_are_detected() {
    // 回环 / 私网 / 链路本地 / 未指定 / v6 唯一本地
    for host in [
        "127.0.0.1",
        "127.8.8.8",
        "::1",
        "10.0.0.5",
        "172.16.1.1",
        "172.31.255.255",
        "192.168.31.1",
        "169.254.1.1",
        "0.0.0.0",
        "fd00::1",
        "fe80::1",
        "localhost",
        "LocalHost",
        "dash.localhost",
    ] {
        assert!(is_local_target(host), "{host} 应判定为本地目标");
    }
}

#[test]
fn public_targets_are_not_local() {
    // 公网 IP / 正常域名不能误判（否则所有代理流量都被强制直连）
    for host in [
        "8.8.8.8",
        "240e:97d:10:1402::1:42",
        "2001:4860::1",
        "::ffff:8.8.8.8",
    ] {
        assert!(!is_local_target(host), "{host} 不应判定为本地目标");
    }
}

#[test]
fn ip_literals_opt_out_of_route_decisions() {
    // 客户端发来的 IP 字面量（尤其公网 IPv6）在本机缺该地址族连通性时
    // 盲拨必然 Network is unreachable；它们没有"域名→路线"这一层，
    // 路线缓存不该对它们做直连/竞速决策。
    for host in [
        "8.8.8.8",
        "240e:97d:10:1402::1:42",
        "2001:4860::1",
        "::ffff:8.8.8.8",
    ] {
        assert!(
            !route_cache_eligible(host, false, false),
            "{host} 不应进入域名级路线决策"
        );
    }
    // 域名正常参与（未钉住、非国内、非本地）。
    assert!(route_cache_eligible("www.gstatic.com", false, false));
    // 三类既有旁路保持不变。
    assert!(!route_cache_eligible("www.baidu.com", false, true)); // 国内
    assert!(!route_cache_eligible("www.gstatic.com", true, false)); // 钉住
    assert!(!route_cache_eligible("localhost", false, false)); // 本地
}

#[tokio::test]
async fn sixteen_byte_domain_is_not_mangled_to_ipv6() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    // "www.bilibili.com" 恰好 16 字节，旧代码按 host_bytes.len()==16
    // 误判为 IPv6，生成 "7777:772e:..." 这样的伪字面量，导致国内后缀表
    // 失配、站点被错误地送进代理候选链。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (mut tx, (mut rx, _)) =
        tokio::join!(async { TcpStream::connect(addr).await.unwrap() }, async {
            listener.accept().await.unwrap()
        },);
    // ATYP=0x03（域名），长度 16
    let domain = b"www.bilibili.com";
    let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
    req.extend_from_slice(domain);
    req.extend_from_slice(&443u16.to_be_bytes());
    tx.write_all(&req).await.unwrap();
    let target = read_socks5_target(&mut rx).await.unwrap();
    assert_eq!(
        target.host, "www.bilibili.com",
        "16 字节域名必须保持域名形态"
    );

    // 对照：真 IPv6（ATYP=0x04）仍正确解码
    let v6 = [
        0x20, 0x01, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
    ];
    let mut req2 = vec![0x05, 0x01, 0x00, 0x04];
    req2.extend_from_slice(&v6);
    req2.extend_from_slice(&443u16.to_be_bytes());
    tx.write_all(&req2).await.unwrap();
    let t2 = read_socks5_target(&mut rx).await.unwrap();
    assert_eq!(t2.host, "2001:4860::1", "真 IPv6 必须解码为字面量");
}
