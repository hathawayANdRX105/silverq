//! MeowMeasurer: 按节点 tag 查 adapter registry，委托 meow-rs 做协议感知测速。
//!
//! silverq 只做调度；连接、握手、TLS/Reality/QUIC 全部由 meow 完成。
//!
//! 两类探测：
//! - **延迟探测**（`measure`）：GET generate_204，只读状态行，全池每轮跑。
//! - **带宽探测**（`measure_bw`）：有限字节下载，读 body 字节数，
//!   只对 selection 跑、每 `bw_interval_rounds` 轮一次。
//!   后者补上前者的盲区：一个握手 700ms 但只有 1Mbit 的节点，
//!   延迟分数比握手 900ms 但 20Mbit 的节点漂亮，真实页面却慢 4 倍。
#![cfg(feature = "meow")]

use crate::proxy::http_probe::{parse_probe_url, read_body_bytes, tls_connect};
use crate::scheduler::batch::Measurer;
use crate::scheduler::node::Node;
use async_trait::async_trait;
use meow_common::adapter::ProxyAdapter;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// 探测 URL 见 `config::probe_url()`（`SILVERQ_PROBE_URL` 可覆盖，供测试指向本地端点）。
pub type Registry = Arc<RwLock<HashMap<String, Arc<dyn ProxyAdapter>>>>;

/// 每个节点一个 meow 协议 adapter（由 factory 构建），按 tag 索引。
/// registry 与数据面 inbound 共享；可用 [`replace`] 在运行时换 adapter。
pub struct MeowMeasurer {
    registry: Registry,
    probe_url: String,
    bw_probe_url: String,
}

impl MeowMeasurer {
    #[cfg_attr(feature = "meow", allow(dead_code))] // 生产走 with_probe；new 供无 env 场景
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            probe_url: crate::config::probe_url(),
            bw_probe_url: crate::config::DEFAULT_BW_PROBE_URL.to_string(),
        }
    }

    /// 生产入口：探测 URL 来自 silverq.toml / env。
    /// 超时不存副本 —— 每次测速由 batch 传参，配置面板热改即时生效
    /// （早先存副本再 min() 合并，热改调大超时会被旧副本盖住）。
    pub fn with_probe(registry: Registry, probe_url: String) -> Self {
        Self {
            registry,
            probe_url,
            bw_probe_url: crate::config::DEFAULT_BW_PROBE_URL.to_string(),
        }
    }

    /// 带宽探测 URL 单独传入：它与延迟探测是不同端点（generate_204 无 body，
    /// 带宽探测需要一个能下发有限字节的端点）。
    pub fn with_bw_probe(registry: Registry, probe_url: String, bw_probe_url: String) -> Self {
        Self {
            registry,
            probe_url,
            bw_probe_url,
        }
    }
}

#[async_trait]
impl Measurer for MeowMeasurer {
    async fn measure(&self, node: &Node, timeout_ms: u64) -> Option<f64> {
        let adapter = {
            let guard = self.registry.read();
            guard.get(&node.tag).cloned()
        }?;

        // meow 协议感知探测：建立真实连接 + 握手 + HTTP GET，返回延迟 ms。
        // 成功返回 Some(delay)；超时/失败返回 None（silverq 按超时扣分后移）。
        match meow_proxy::health::url_test(
            adapter.as_ref(),
            &self.probe_url,
            Some("200,204"),
            Duration::from_millis(timeout_ms),
        )
        .await
        {
            Ok(delay) => Some(delay as f64),
            Err(_) => None,
        }
    }

    async fn measure_bw(&self, node: &Node, _timeout_ms: u64, max_bytes: u64) -> Option<f64> {
        let adapter = {
            let guard = self.registry.read();
            guard.get(&node.tag).cloned()
        }?;

        // 复用延迟探测同款拨号路径（adapter 自己处理协议握手 + 可选 TLS），
        // 但读 body 字节而非只读状态行。超时由 run_bw_batch 的 tokio::timeout 兜底。
        let parsed = parse_probe_url(&self.bw_probe_url)?;
        let metadata = meow_common::Metadata {
            network: meow_common::Network::Tcp,
            host: parsed.host.as_str().into(),
            dst_port: parsed.port,
            ..Default::default()
        };
        let conn = adapter.dial_tcp(&metadata).await.ok()?;
        if parsed.https {
            let tls = tls_connect(parsed.host.as_str(), conn).await?;
            read_body_bytes(tls, &parsed, max_bytes)
                .await
                .map(|b| b as f64)
        } else {
            read_body_bytes(conn, &parsed, max_bytes)
                .await
                .map(|b| b as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bw_probe_url() {
        let u = parse_probe_url("https://speed.cloudflare.com/__down?bytes=524288").unwrap();
        assert!(u.https);
        assert_eq!(u.host, "speed.cloudflare.com");
        assert_eq!(u.port, 443);
    }
}
