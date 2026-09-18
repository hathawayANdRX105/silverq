#![cfg(feature = "meow")]

use silverq::proxy::factory::*;
use silverq::proxy::nodespec::NodeSpec;

fn vless_spec(tls: bool, transport: Option<silverq::proxy::nodespec::TransportSpec>) -> NodeSpec {
    NodeSpec {
        tag: "T".into(),
        protocol: silverq::proxy::nodespec::Protocol::Vless,
        server: "1.2.3.4".into(),
        port: 443,
        vless: Some(silverq::proxy::nodespec::VlessSpec {
            uuid: "b85798ef-9edc-46a4-9a87-8da4499d36d0".into(),
            sni: Some("example.com".into()),
            flow: None,
            reality: None,
            fingerprint: None,
            tls,
            transport,
        }),
        trojan: None,
        shadowsocks: None,
        hysteria2: None,
    }
}

/// 层序是硬约束：TLS 必须贴 TCP、ws/grpc 叠在其上（meow TransportChain 文档：
/// VLESS over WS over TLS）。搞反了节点全连不上，而这种错不会编译失败。
/// 这里锁层数，顺序错位在真实节点上会表现为握手失败。
#[test]
fn transport_chain_layer_count() {
    use silverq::proxy::nodespec::TransportSpec;

    // 纯 TLS：1 层
    let spec = vless_spec(true, None);
    let v = spec.vless.as_ref().unwrap();
    assert_eq!(build_transport(&spec, v).unwrap().len(), 1);

    // TLS + ws：2 层
    let spec = vless_spec(
        true,
        Some(TransportSpec::Ws {
            path: "/x".into(),
            host: None,
        }),
    );
    let v = spec.vless.as_ref().unwrap();
    assert_eq!(build_transport(&spec, v).unwrap().len(), 2);

    // 明文 ws（无 TLS）：1 层 —— 真实池里有这种节点
    let spec = vless_spec(
        false,
        Some(TransportSpec::Ws {
            path: "/x".into(),
            host: Some("cdn.example.com".into()),
        }),
    );
    let v = spec.vless.as_ref().unwrap();
    assert_eq!(build_transport(&spec, v).unwrap().len(), 1);

    // TLS + grpc：2 层
    let spec = vless_spec(
        true,
        Some(TransportSpec::Grpc {
            service_name: "update".into(),
        }),
    );
    let v = spec.vless.as_ref().unwrap();
    assert_eq!(build_transport(&spec, v).unwrap().len(), 2);

    // 无 TLS 无 transport：0 层（裸 TCP）
    let spec = vless_spec(false, None);
    let v = spec.vless.as_ref().unwrap();
    assert!(build_transport(&spec, v).unwrap().is_empty());
}

#[test]
fn uuid_parse() {
    assert_eq!(
        parse_uuid("b85798ef-9edc-46a4-9a87-8da4499d36d0").unwrap(),
        [
            0xb8, 0x57, 0x98, 0xef, 0x9e, 0xdc, 0x46, 0xa4, 0x9a, 0x87, 0x8d, 0xa4, 0x49, 0x9d,
            0x36, 0xd0
        ]
    );
    assert!(parse_uuid("garbage").is_err());
}

/// 回归：Reality 公钥的真实形态是 43 字符无 padding base64url。
/// 曾只试带 padding 的引擎，导致真实节点池里 116/179 个节点建不出 adapter
/// （报 Invalid symbol 95，即 `_`）。
#[test]
fn reality_key_accepts_unpadded_base64url() {
    // 43 字符、含 `-` 与 `_`、无 padding —— 与真实节点里的形态一致
    let k = "uoXBNYcBgR-h2YmU1NFhFyASr6qh9UghWOuo1WZIikw";
    assert_eq!(k.len(), 43);
    let decoded = base64url_decode32(k).expect("无 padding base64url 必须能解");
    assert_eq!(decoded.len(), 32);

    // 带 padding 的标准 base64 也要继续能解（不同订阅源格式不一）
    use base64::Engine;
    let padded = base64::engine::general_purpose::STANDARD.encode(decoded);
    assert_eq!(base64url_decode32(&padded).unwrap(), decoded);

    // 解出来不是 32 字节要报错，而不是静默截断
    let short = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8; 16]);
    assert!(base64url_decode32(&short).is_err(), "长度不对必须报错");
    assert!(base64url_decode32("!!!not base64!!!").is_err());
}

#[test]
fn short_id_pad() {
    assert_eq!(
        hex_to_short_id("17d824dd68e24aaf").unwrap(),
        [0x17, 0xd8, 0x24, 0xdd, 0x68, 0xe2, 0x4a, 0xaf]
    );
    assert_eq!(hex_to_short_id("aa").unwrap(), [0xaa, 0, 0, 0, 0, 0, 0, 0]);
}
