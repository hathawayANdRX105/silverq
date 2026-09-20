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
    /// 拨号地址：load 后由真实上游 DNS 解析 `server` 域名填充（见
    /// [`crate::proxy::dns`]——TUN + fake-IP 环境下系统解析返回假地址，
    /// 会把 silverq 自己的节点拨号劫进自家隧道）。None = 未解析，
    /// build_proxy 回退用 `server` 原文。运行时派生态，不序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dial_addr: Option<String>,
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
    /// 是否启用 TLS。ws/grpc 节点可能是明文 + 仅靠传输层伪装。
    #[serde(default = "default_true")]
    pub tls: bool,
    /// 传输层（ws / grpc）。None = 裸 TCP。
    #[serde(default)]
    pub transport: Option<TransportSpec>,
}

fn default_true() -> bool {
    true
}

/// 传输层配置。层序固定：TLS 贴 TCP，ws/grpc 叠在 TLS 之上
/// （见 meow 的 TransportChain 文档：VLESS over WS over TLS）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportSpec {
    Ws {
        #[serde(default = "default_ws_path")]
        path: String,
        /// Host 头。留空则用 SNI / server。
        #[serde(default)]
        host: Option<String>,
    },
    Grpc {
        #[serde(default = "default_grpc_service")]
        service_name: String,
    },
}

fn default_ws_path() -> String {
    "/".to_string()
}

fn default_grpc_service() -> String {
    "GunService".to_string()
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
    #[serde(default)]
    pub obfs: Option<String>,
    #[serde(default)]
    pub obfs_password: Option<String>,
    #[serde(default)]
    pub ports: Option<String>,
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
///
/// 按 tag 去重（保留首次出现）。tag 同时是 registry 的 key 和 selection 的元素：
/// 上游 pool poller 每轮「剥旧 POOL- 行 + 灌新行」时会写出重复 tag（实测 880 行 /
/// 813 唯一，单个 tag 最多 6 份）。不去重则前 N 名被同一个节点重复占位，
/// fallback 退化成本节点重试——selection 10 个名额只覆盖 3 个真实节点。
pub fn load_nodes_yaml(path: &str) -> Result<Vec<NodeSpec>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path, e))?;
    let file: NodeFile =
        serde_yaml::from_str(&raw).map_err(|e| format!("parse {}: {}", path, e))?;
    let mut seen = std::collections::HashSet::new();
    let mut nodes = Vec::with_capacity(file.nodes.len());
    for n in file.nodes {
        n.validate()?;
        if seen.insert(n.tag.clone()) {
            nodes.push(n);
        }
    }
    Ok(nodes)
}
