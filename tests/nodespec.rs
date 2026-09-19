use silverq::proxy::nodespec::*;

#[test]
fn parse_full_yaml() {
    let yaml = r#"
nodes:
  - tag: "A"
    protocol: vless
    server: "1.2.3.4"
    port: 443
    vless:
      uuid: "00000000-0000-0000-0000-000000000000"
      sni: "www.cloudflare.com"
      reality:
        public_key: "abc"
        short_id: "17d824dd68e24aaf"
  - tag: "B"
    protocol: shadowsocks
    server: "5.6.7.8"
    port: 8388
    shadowsocks:
      password: "pw"
"#;
    let file: NodeFile = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(file.nodes.len(), 2);
    assert!(file.nodes[0].validate().is_ok());
    assert!(file.nodes[1].validate().is_ok());
}

#[test]
fn parses_ws_and_grpc_transport() {
    let yaml = r#"
nodes:
  - tag: "WS"
    protocol: vless
    server: "1.2.3.4"
    port: 8080
    vless:
      uuid: "00000000-0000-0000-0000-000000000000"
      tls: false
      transport:
        type: "ws"
        path: "/?ed=2560"
        host: "cdn.example.com"
  - tag: "GRPC"
    protocol: vless
    server: "5.6.7.8"
    port: 443
    vless:
      uuid: "00000000-0000-0000-0000-000000000000"
      transport:
        type: "grpc"
        service_name: "update"
"#;
    let file: NodeFile = serde_yaml::from_str(yaml).unwrap();
    let ws = file.nodes[0].vless.as_ref().unwrap();
    assert!(!ws.tls, "tls: false 必须被读到（明文 ws 节点）");
    match ws.transport.as_ref().unwrap() {
        TransportSpec::Ws { path, host } => {
            assert_eq!(path, "/?ed=2560");
            assert_eq!(host.as_deref(), Some("cdn.example.com"));
        }
        other => panic!("期望 ws，得到 {other:?}"),
    }

    let g = file.nodes[1].vless.as_ref().unwrap();
    assert!(g.tls, "未写 tls 时应默认 true");
    match g.transport.as_ref().unwrap() {
        TransportSpec::Grpc { service_name } => assert_eq!(service_name, "update"),
        other => panic!("期望 grpc，得到 {other:?}"),
    }
}

#[test]
fn missing_creds_rejected() {
    let yaml = r#"
nodes:
  - tag: "C"
    protocol: trojan
    server: "9.9.9.9"
    port: 443
"#;
    let file: NodeFile = serde_yaml::from_str(yaml).unwrap();
    assert!(file.nodes[0].validate().is_err());
}

/// 重复 tag 必须去重：tag 是 registry 的 key 也是 selection 的元素，
/// 池里同一个 tag 出现 N 次会让前 N 名被同一个节点重复占位 ——
/// fallback 退化成本节点重试，selection 的 10 个名额只覆盖 3 个真实节点。
/// 上游 pool poller 实测会写出这种表（880 行 / 813 唯一，单 tag 最多 6 份）。
#[test]
fn duplicate_tags_are_deduped_on_load() {
    let yaml = r#"
nodes:
  - tag: "POOL-A"
    protocol: vless
    server: "1.1.1.1"
    port: 443
    vless:
      uuid: "00000000-0000-0000-0000-000000000000"
  - tag: "POOL-B"
    protocol: shadowsocks
    server: "2.2.2.2"
    port: 8388
    shadowsocks:
      password: "pw"
  - tag: "POOL-A"
    protocol: vless
    server: "1.1.1.1"
    port: 443
    vless:
      uuid: "00000000-0000-0000-0000-000000000000"
  - tag: "POOL-A"
    protocol: vless
    server: "1.1.1.1"
    port: 443
    vless:
      uuid: "00000000-0000-0000-0000-000000000000"
"#;
    let path = std::env::temp_dir().join(format!(
        "silverq-nodespec-dedupe-{}-{:?}.yaml",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, yaml).unwrap();
    let nodes = load_nodes_yaml(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);

    let tags: Vec<&str> = nodes.iter().map(|n| n.tag.as_str()).collect();
    assert_eq!(tags, vec!["POOL-A", "POOL-B"], "同 tag 只保留首次出现");
}

/// 重复行若凭证不同（同 tag 不同 server），仍以首次出现的为准 ——
/// 去重是为了让 tag 唯一，不是为了合并节点。
#[test]
fn duplicate_tag_keeps_first_row() {
    let yaml = r#"
nodes:
  - tag: "DUP"
    protocol: vless
    server: "1.1.1.1"
    port: 443
    vless:
      uuid: "00000000-0000-0000-0000-000000000000"
  - tag: "DUP"
    protocol: vless
    server: "9.9.9.9"
    port: 8443
    vless:
      uuid: "11111111-1111-1111-1111-111111111111"
"#;
    let path = std::env::temp_dir().join(format!(
        "silverq-nodespec-first-{}-{:?}.yaml",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, yaml).unwrap();
    let nodes = load_nodes_yaml(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].server, "1.1.1.1");
    assert_eq!(nodes[0].port, 443);
}
