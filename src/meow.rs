//! MeowMeasurer: 按节点 tag 查 adapter registry，委托 meow-rs 做协议感知测速。
//!
//! lift 只做调度；连接、握手、TLS/Reality/QUIC 全部由 meow 完成。
#![cfg(feature = "meow")]

use crate::batch::Measurer;
use crate::node::Node;
use async_trait::async_trait;
use meow_common::adapter::ProxyAdapter;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// 探测 URL 见 `config::probe_url()`（`LIFT_PROBE_URL` 可覆盖，供测试指向本地端点）。
pub type Registry = Arc<RwLock<HashMap<String, Arc<dyn ProxyAdapter>>>>;

/// 每个节点一个 meow 协议 adapter（由 factory 构建），按 tag 索引。
/// registry 与数据面 inbound 共享；可用 [`replace`] 在运行时换 adapter。
pub struct MeowMeasurer {
    registry: Registry,
}

impl MeowMeasurer {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
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
        // 成功返回 Some(delay)；超时/失败返回 None（lift 按超时扣分后移）。
        match meow_proxy::health::url_test(
            adapter.as_ref(),
            &crate::config::probe_url(),
            Some("200,204"),
            Duration::from_millis(timeout_ms),
        )
        .await
        {
            Ok(delay) => Some(delay as f64),
            Err(_) => None,
        }
    }
}
