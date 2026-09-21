//! Batch concurrent measurement with fast/slow path separation.
use crate::scheduler::node::Node;
use async_trait::async_trait;
use std::time::Duration;
use tokio::time::timeout;

pub struct Measurement {
    pub tag: String,
    pub delay_ms: Option<f64>, // None = timeout / failure
    /// 带宽采样（bytes/sec）。None = 未探测或探测失败。
    /// 与延迟独立：一个节点可以握手快（低延迟）但带宽只有 1Mbit，
    /// 只测延迟会把这类「快但慢」的节点排在队首。
    pub bw_bps: Option<f64>,
}

/// Trait for performing delay measurement on a node.
/// Implementations:
/// - HttpDelayMeasurer (for sing-box clash_api style, migration path)
/// - MeowMeasurer (direct meow-rs / meow-proxy integration)
#[async_trait]
pub trait Measurer: Send + Sync {
    /// Timeout nodes will be heavily penalized and moved to the back.
    async fn measure(&self, node: &Node, timeout_ms: u64) -> Option<f64>;

    /// 有限字节下载探测：返回 bytes/sec。None = 失败/超时。
    ///
    /// 只在 selection 内的节点上跑（每 N 轮一次），不对全池跑：
    /// 512KB × 40 节点 × 每轮 = 80MB/轮探测流量，免费节点撑不住，
    /// 也会跟用户真实流量抢带宽。
    async fn measure_bw(&self, node: &Node, timeout_ms: u64, max_bytes: u64) -> Option<f64>;
}

/// 空测速器：默认（无 meow feature）模式用，恒定返回 100ms。
/// 用途是脱离 meow 自测调度逻辑本身（分批、EWMA、选择、切换）。
#[cfg_attr(feature = "meow", allow(dead_code))]
#[derive(Clone)]
pub struct NoopMeasurer;

#[async_trait]
impl Measurer for NoopMeasurer {
    async fn measure(&self, _node: &Node, _timeout_ms: u64) -> Option<f64> {
        Some(100.0)
    }
    async fn measure_bw(&self, _node: &Node, _timeout_ms: u64, _max_bytes: u64) -> Option<f64> {
        None // 无 meow 时不做带宽探测；score 里带宽项保持 0（不奖不罚）
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
                    bw_bps: None,
                },
                _ => Measurement {
                    tag: node.tag.clone(),
                    delay_ms: None,
                    bw_bps: None,
                },
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    results
}

/// 对 selection 内的节点跑有限字节下载，测吞吐（bytes/sec）。
///
/// 与延迟批分离：延迟批全池每轮跑，吞吐批只跑 selection 且每 N 轮一次
/// （`bw_interval_rounds`）。结果经 `apply_bw_batch` 写回节点的对数域 EWMA。
pub async fn run_bw_batch<M: Measurer + ?Sized>(
    measurer: &M,
    nodes: Vec<Node>,
    timeout_ms: u64,
    max_bytes: u64,
    concurrency: usize,
) -> Vec<Measurement> {
    stream::iter(nodes)
        .map(|node| async move {
            let bps = timeout(
                Duration::from_millis(timeout_ms),
                measurer.measure_bw(&node, timeout_ms, max_bytes),
            )
            .await
            .ok()
            .flatten();
            Measurement {
                tag: node.tag.clone(),
                delay_ms: None,
                bw_bps: bps,
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await
}
