//! ProxyFactory: NodeSpec -> Arc<dyn Proxy>，复用 meow-rs 的 adapter。
//! lift 不实现任何协议，只负责把节点规格喂给 meow。
#![cfg(feature = "meow")]

use meow_common::adapter::ProxyAdapter;
use meow_proxy::{
    Hy2Adapter, Hy2Options, ShadowsocksAdapter, TransportChain, TrojanAdapter, VlessAdapter,
    VlessFlow,
};
use meow_transport::tls::{RealityConfig, TlsConfig};
use std::sync::Arc;

use crate::nodespec::NodeSpec;

/// 从节点规格构建 meow Proxy（协议 adapter）。
pub fn build_proxy(spec: &NodeSpec) -> Result<Arc<dyn ProxyAdapter>, String> {
    let udp = true; // UDP 保留给数据面；调度测速走 TCP 探测

    let proxy: Box<dyn ProxyAdapter> = match spec.protocol {
        crate::nodespec::Protocol::Vless => {
            let v = spec.vless.as_ref().ok_or("vless spec missing")?;
            let uuid_bytes = parse_uuid(v.uuid.as_str())?;
            let flow = if v.flow.as_deref() == Some("xtls-rprx-vision") {
                Some(VlessFlow::XtlsRprxVision)
            } else {
                None
            };
            let transport = build_transport(
                spec,
                v.sni.as_deref(),
                v.reality.clone(),
                v.fingerprint.clone(),
            )?;
            Box::new(VlessAdapter::new(
                spec.tag.as_str(),
                spec.server.as_str(),
                spec.port,
                uuid_bytes,
                flow,
                udp,
                transport,
            ))
        }
        crate::nodespec::Protocol::Trojan => {
            let t = spec.trojan.as_ref().ok_or("trojan spec missing")?;
            let sni = t.sni.clone().unwrap_or_else(|| spec.server.clone());
            Box::new(TrojanAdapter::new(
                spec.tag.as_str(),
                spec.server.as_str(),
                spec.port,
                t.password.as_str(),
                sni.as_str(),
                t.skip_cert_verify,
                udp,
            ))
        }
        crate::nodespec::Protocol::Shadowsocks => {
            let s = spec
                .shadowsocks
                .as_ref()
                .ok_or("shadowsocks spec missing")?;
            Box::new(
                ShadowsocksAdapter::new(
                    spec.tag.as_str(),
                    spec.server.as_str(),
                    spec.port,
                    s.password.as_str(),
                    s.cipher.as_str(),
                    udp,
                    None,
                    None,
                )
                .map_err(|e| e.to_string())?,
            )
        }
        crate::nodespec::Protocol::Hysteria2 => {
            let h = spec.hysteria2.as_ref().ok_or("hysteria2 spec missing")?;
            let options = Hy2Options {
                name: spec.tag.clone(),
                server: spec.server.clone(),
                port: spec.port,
                password: h.password.clone(),
                sni: h.sni.clone(),
                skip_cert_verify: true,
                udp,
                up_bps: 0,
                down_bps: 0,
                obfs: None,
                obfs_password: None,
                ports: None,
                hop_interval: None,
                fingerprint: None,
                fast_open: false,
            };
            Box::new(Hy2Adapter::new(options).map_err(|e| e.to_string())?)
        }
        crate::nodespec::Protocol::Direct => Box::new(meow_proxy::DirectAdapter::new()),
    };

    Ok(Arc::from(proxy))
}

/// VLESS 的 transport chain：Reality 优先，否则普通 TLS。
fn build_transport(
    spec: &NodeSpec,
    sni: Option<&str>,
    reality: Option<crate::nodespec::RealitySpec>,
    fingerprint: Option<String>,
) -> Result<TransportChain, String> {
    let mut chain = TransportChain::empty();
    let sni = sni.unwrap_or(spec.server.as_str()).to_string();

    let mut tls = TlsConfig {
        enabled: true,
        sni: Some(sni),
        alpn: Vec::new(),
        skip_cert_verify: true,
        client_cert: None,
        fingerprint,
        additional_roots: Vec::new(),
        ech: None,
        reality: None,
    };

    if let Some(r) = reality {
        let mut rcfg = RealityConfig {
            public_key: base64url_decode32(&r.public_key)?,
            short_id: [0u8; 8],
            support_x25519_mlkem768: false,
        };
        if let Some(sid) = r.short_id {
            rcfg.short_id = hex_to_short_id(&sid)?;
        }
        tls.reality = Some(rcfg);
        tls.skip_cert_verify = false; // Reality 路径自己做认证
    }

    let layer = meow_transport::tls::TlsLayer::new(&tls)
        .map_err(|e| format!("{}: build TlsLayer failed: {e}", spec.tag))?;
    chain.push(Box::new(layer));
    Ok(chain)
}

/// UUID 字符串 -> [u8; 16]（不引 uuid 依赖，剥 dash 后按 16 字节取）。
fn parse_uuid(s: &str) -> Result<[u8; 16], String> {
    let hex: String = s.replace('-', "");
    if hex.len() != 32 {
        return Err(format!("bad uuid: {s}"));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("bad uuid hex in {s}"))?;
    }
    Ok(out)
}

/// 解码 Reality 公钥（32 字节）。
///
/// Reality 公钥的实际形态是 **43 字符无 padding base64url**（含 `-` / `_`）。
/// 只用带 padding 的引擎会全线失败：真实节点池里 116/179 个节点曾因此
/// 报 `Invalid symbol 95`（95 = `_`）。四种变体全试，顺序按出现频率。
fn base64url_decode32(s: &str) -> Result<[u8; 32], String> {
    use base64::Engine;
    let s = s.trim();
    let engines: [&base64::engine::GeneralPurpose; 4] = [
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::STANDARD,
    ];
    let bytes = engines
        .iter()
        .find_map(|e| e.decode(s).ok())
        .ok_or_else(|| format!("public_key 不是合法 base64（len={}）", s.len()))?;

    if bytes.len() != 32 {
        return Err(format!(
            "reality public_key must decode to 32 bytes, got {}",
            bytes.len()
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_to_short_id(s: &str) -> Result<[u8; 8], String> {
    let hex = s.trim();
    if hex.len() > 16 {
        return Err(format!("short_id too long: {hex}"));
    }
    let padded = format!("{hex:0<16}");
    let mut out = [0u8; 8];
    for i in 0..8 {
        out[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("bad short_id hex: {s}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
