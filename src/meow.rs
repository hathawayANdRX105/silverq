//! MeowMeasurer - thin adapter that delegates delay measurement to meow-rs.
//!
//! lift 只负责调度，不处理任何协议细节。
//! 所有 outbound 创建、TLS、Reality、Hysteria2 等全部由 meow-rs 负责。
#![cfg(feature = "meow")]

use crate::batch::Measurer;
use crate::node::Node;
use async_trait::async_trait;

/// MeowMeasurer simply forwards measurement requests to meow-rs.
/// The actual connection and timing logic lives inside meow-proxy / meow-transport.
pub struct MeowMeasurer {
    // In real implementation, this would hold a reference or factory
    // that can produce meow outbounds for different nodes.
    // For now it is just a marker.
}

impl MeowMeasurer {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Measurer for MeowMeasurer {
    /// Delegate to meow-rs to measure the node.
    /// Returns None if meow reports timeout or failure.
    async fn measure(&self, _node: &Node, _timeout_ms: u64) -> Option<f64> {
        // TODO: call into meow-proxy to create outbound for this node and measure RTT
        // Example direction:
        //   let outbound = meow_outbound_factory.create_for_node(node);
        //   let start = Instant::now();
        //   outbound.connect().await?;
        //   Some(start.elapsed().as_millis() as f64)
        None
    }
}
