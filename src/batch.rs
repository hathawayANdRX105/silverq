//! Batch concurrent measurement with fast/slow path separation.
use async_trait::async_trait;
use crate::node::Node;
use std::time::Duration;
use tokio::time::timeout;

pub struct Measurement {
    pub tag: String,
    pub delay_ms: Option<f64>, // None = timeout / failure
}

/// Trait for performing delay measurement on a node.
/// Implementations:
/// - HttpDelayMeasurer (for sing-box clash_api style, migration path)
/// - MeowMeasurer (direct meow-rs / meow-proxy integration)
#[async_trait]
pub trait Measurer: Send + Sync {
    /// Timeout nodes will be heavily penalized and moved to the back.
    async fn measure(&self, node: &Node, timeout_ms: u64) -> Option<f64>;
}

/// Placeholder measurer for testing (always returns a stable value).
#[derive(Clone)]
pub struct NoopMeasurer;

#[async_trait]
impl Measurer for NoopMeasurer {
    async fn measure(&self, _node: &Node, _timeout_ms: u64) -> Option<f64> {
        Some(100.0)
    }
}

// TODO: implement MeowMeasurer using meow-proxy + meow-transport
// TODO: implement HttpDelayMeasurer that calls /proxies/{tag}/delay (for compatibility)

// Example stub for meow-rs integration (to be implemented):
// use meow_proxy::...;
// pub struct MeowMeasurer {
//     // meow outbound config / transport
// }
// #[async_trait]
// impl Measurer for MeowMeasurer {
//     async fn measure(&self, node: &Node, timeout_ms: u64) -> Option<f64> {
//         // TODO: use meow-proxy to establish outbound to node.server:node.port
//         // and measure handshake / first packet RTT
//         None
//     }
// }

// Example stub for HTTP /delay style (sing-box compatible, for migration):
// pub struct HttpDelayMeasurer { /* api base, secret */ }
// #[async_trait]
// impl Measurer for HttpDelayMeasurer {
//     async fn measure(&self, node: &Node, timeout_ms: u64) -> Option<f64> {
//         // TODO: call http://127.0.0.1:9090/proxies/{node.tag}/delay
//         None
//     }
// }

use futures_util::stream::{self, StreamExt};

pub async fn run_batch_owned<M: Measurer + ?Sized>(
    measurer: &M,
    nodes: Vec<Node>,
    timeout_ms: u64,
    concurrency: usize,
) -> Vec<Measurement> {
    let results: Vec<Measurement> = stream::iter(nodes)
        .map(|node| async move {
            let res = timeout(
                Duration::from_millis(timeout_ms),
                measurer.measure(&node, timeout_ms),
            )
            .await;

            match res {
                Ok(Some(delay)) => Measurement {
                    tag: node.tag.clone(),
                    delay_ms: Some(delay),
                },
                _ => Measurement {
                    tag: node.tag.clone(),
                    delay_ms: None,
                },
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    results
}
