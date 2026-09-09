//! 节点配置模型：YAML 节点表 + 协议字段。
//!
//! 示例 (nodes.yaml):
//! ```yaml
//! - tag: "WB-JP-01"
//!   protocol: vless
//!   server: "1.2.3.4"
//!   port: 443
//!   vless:
//!     uuid: "..."
//!     sni: "www.cloudflare.com"
//!     reality:
//!       public_key: "..."
//!       short_id: "17d824dd68e24aaf"
//! ```
use serde::{Deserialize, Serialize};

/// 顶层节点表文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeFile {
    pub nodes: Vec<NodeSpec>,
}

/// 单个节点定义（协议 + 凭证）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    pub tag: String,
    pub protocol: Protocol,
    pub server: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vless: Option<VlessSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trojan: Option<TrojanSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadowsocks: Option<ShadowsocksSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hysteria2: Option<Hysteria2Spec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Vless,
    Trojan,
    Shadowsocks,
    Hysteria2,
    /// 直连（无代理）。用作基线对照：测出的延迟就是不走代理的延迟。
    Direct,
}

/// VLESS 凭证（Reality 可选内嵌）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VlessSpec {
    pub uuid: String,
    /// SNI。没有时默认用 server。
    #[serde(default)]
    pub sni: Option<String>,
    #[serde(default)]
    pub flow: Option<String>, // "xtls-rprx-vision"
    #[serde(default)]
    pub reality: Option<RealitySpec>,
    /// TLS 指纹
    #[serde(default)]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealitySpec {
    pub public_key: String,
    #[serde(default)]
    pub short_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrojanSpec {
    pub password: String,
    /// SNI。没有时默认用 server。
    #[serde(default)]
    pub sni: Option<String>,
    /// 服务器证书校验模式。MVP 默认跳过 pinning。
    #[serde(default = "default_skip_cert_verify")]
    pub skip_cert_verify: bool,
}

fn default_skip_cert_verify() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowsocksSpec {
    pub password: String,
    /// "2022-blake3-aes-256-gcm" 等
    #[serde(default = "default_ss_cipher")]
    pub cipher: String,
}

fn default_ss_cipher() -> String {
    "2022-blake3-aes-256-gcm".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hysteria2Spec {
    pub password: String,
    #[serde(default)]
    pub sni: Option<String>,
}

impl NodeSpec {
    /// 校验：协议字段与凭证匹配。
    pub fn validate(&self) -> Result<(), String> {
        match self.protocol {
            Protocol::Vless => {
                self.vless
                    .as_ref()
                    .ok_or_else(|| format!("{}: vless protocol needs vless spec", self.tag))?;
            }
            Protocol::Trojan => {
                self.trojan
                    .as_ref()
                    .ok_or_else(|| format!("{}: trojan protocol needs trojan spec", self.tag))?;
            }
            Protocol::Shadowsocks => {
                self.shadowsocks.as_ref().ok_or_else(|| {
                    format!("{}: shadowsocks protocol needs shadowsocks spec", self.tag)
                })?;
            }
            Protocol::Hysteria2 => {
                self.hysteria2.as_ref().ok_or_else(|| {
                    format!("{}: hysteria2 protocol needs hysteria2 spec", self.tag)
                })?;
            }
            // 直连无凭证可校验
            Protocol::Direct => {}
        }
        Ok(())
    }
}

/// 从 YAML 文件加载节点表。
pub fn load_nodes_yaml(path: &str) -> Result<Vec<NodeSpec>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path, e))?;
    let file: NodeFile =
        serde_yaml::from_str(&raw).map_err(|e| format!("parse {}: {}", path, e))?;
    for n in &file.nodes {
        n.validate()?;
    }
    Ok(file.nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
