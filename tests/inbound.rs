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
    // adapter 的 name() 是协议内置名（DIRECT 之类），所以按位置对应
    // order 里的 tag 来断言，而不是比名字。
    let got = pick_candidates(&order, &map, 2);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name(), map["a"].name());
    assert_eq!(got[1].name(), map["b"].name());

    // 上限 1：只有队首
    assert_eq!(pick_candidates(&order, &map, 1).len(), 1);

    // 上限大：全部存在节点（missing 跳过）
    assert_eq!(pick_candidates(&order, &map, 10).len(), 4);

    // 上限 0：至少保底 1 个（max(1)），完全不放行会让请求必死
    assert_eq!(pick_candidates(&order, &map, 0).len(), 1);

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
