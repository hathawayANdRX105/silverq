//! DNS 解析（src/proxy/dns.rs）：报文编解码纯函数 + 中国域名直连表 +
//! 节点域名预解析的无网可测部分。

use silverq::proxy::dns::{
    build_query, load_china_domains, parse_answers, resolve_dial_addrs, resolve_host, ChinaSet,
};
use silverq::proxy::nodespec::{NodeSpec, Protocol};
use std::net::IpAddr;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

// example.com @223.5.5.5 的真实响应（2026-09-19 抓取，含两个答案 +
// 0xC0 名字指针压缩），ID = 0xabcd。
const A_RESP: &str = "abcd81800001000200000000076578616d706c6503636f6d0000\
    010001c00c000100010000008d0004ac4293f3c00c000100010000008d00046814179a";
const AAAA_RESP: &str = "abcd81800001000200000000076578616d706c6503636f6d0000\
    1c0001c00c001c00010000001300102606470000100000000000006814179a\
    c00c001c0001000000130010260647000010000000ac4293f3";

#[test]
fn parse_a_response_extracts_all_answers() {
    let ips = parse_answers(&hex(A_RESP), 1, 0xabcd);
    assert_eq!(ips.len(), 2);
    assert!(ips.contains(&"172.66.147.243".parse::<IpAddr>().unwrap()));
    assert!(ips.contains(&"104.20.23.154".parse::<IpAddr>().unwrap()));
}

#[test]
fn parse_aaaa_response_extracts_all_answers() {
    let ips = parse_answers(&hex(AAAA_RESP), 28, 0xabcd);
    assert_eq!(ips.len(), 2);
    assert!(ips.contains(&"2606:4700:10::6814:179a".parse::<IpAddr>().unwrap()));
    assert!(ips.contains(&"2606:4700:10::ac42:93f3".parse::<IpAddr>().unwrap()));
}

#[test]
fn parse_rejects_mismatched_transaction_id() {
    let mut bytes = hex(A_RESP);
    bytes[0] ^= 0xff;
    assert!(parse_answers(&bytes, 1, 0xabcd).is_empty());
}

#[test]
fn build_query_encodes_name_and_type() {
    let q = build_query(0x1234, "a.example.com", 28).expect("valid name");
    assert_eq!(&q[0..2], &[0x12u8, 0x34]);
    // QNAME: 1"a" 7"example" 3"com" 0
    let expect: &[u8] = b"\x01a\x07example\x03com\x00";
    assert!(q.windows(expect.len()).any(|w| w == expect));
    // QTYPE=AAAA(28) IN(1) 收尾
    assert_eq!(&q[q.len() - 4..], &[0x00u8, 28, 0x00, 0x01]);
}

#[test]
fn build_query_rejects_bad_names() {
    assert!(build_query(1, "", 1).is_none());
    assert!(build_query(1, "a..b", 1).is_none());
    assert!(build_query(1, "ok.example.", 1).is_none());
    assert!(build_query(1, &"x".repeat(64), 1).is_none());
}

#[test]
fn china_set_matches_suffix_not_substring() {
    let cs = ChinaSet::new(&["example.com".into(), "baidu.com".into()]);
    assert!(cs.matches("example.com"));
    assert!(cs.matches("a.example.com"));
    assert!(cs.matches("EXAMPLE.com."));
    assert!(cs.matches("www.baidu.com"));
    assert!(!cs.matches("notexample.com"), "子串不能误判为后缀命中");
    assert!(!cs.matches("baidu.cn"));
    assert!(!cs.matches("example.org"));
}

#[test]
fn load_china_domains_skips_comments_and_blank() {
    let dir = std::env::temp_dir().join("silverq-china-test");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("domains.txt");
    std::fs::write(&p, "a.com\n\n# comment\n b.com \n").unwrap();
    assert_eq!(
        load_china_domains(&p),
        vec!["a.com".to_string(), "b.com".to_string()]
    );
    assert!(load_china_domains(&dir.join("missing.txt")).is_empty());
}

#[test]
fn resolve_host_passes_through_ip_literals() {
    // 字面量不发包：127.0.0.1 在无网环境也必须原样返回。
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ip = rt.block_on(resolve_host("127.0.0.1"));
    assert_eq!(ip, Some("127.0.0.1".parse().unwrap()));
}

#[test]
fn resolve_dial_addrs_fills_only_domain_nodes() {
    // 字面量节点不该有 dial_addr（无域名可解）；域名节点用假上游
    // （127.0.0.1:53 无服务，连接即拒）→ 解析失败保持 None（回退旧行为）。
    std::env::set_var("SILVERQ_RESOLVE_UPSTREAMS", "127.0.0.1");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut specs = vec![
        NodeSpec {
            tag: "ip-node".into(),
            protocol: Protocol::Shadowsocks,
            server: "127.0.0.1".into(),
            port: 1,
            dial_addr: None,
            vless: None,
            trojan: None,
            shadowsocks: None,
            hysteria2: None,
        },
        NodeSpec {
            tag: "domain-node".into(),
            protocol: Protocol::Shadowsocks,
            server: "invalid.invalid".into(),
            port: 1,
            dial_addr: None,
            vless: None,
            trojan: None,
            shadowsocks: None,
            hysteria2: None,
        },
    ];
    rt.block_on(resolve_dial_addrs(&mut specs));
    assert!(specs[0].dial_addr.is_none(), "IP 字面量无需预解析");
    assert!(
        specs[1].dial_addr.is_none(),
        "解析失败应回退系统解析（dial_addr 保持 None）"
    );
}
