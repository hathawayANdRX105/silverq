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
