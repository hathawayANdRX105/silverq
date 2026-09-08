//! Slow path: periodic full-pool offline measurement.
//! Runs independently in background.
//! Uses ranked order for batching (best nodes first).
//! Timeout nodes are penalized and moved toward the back.
//! This module is kept for backwards compatibility.
//! In current main.rs, the slow path loop is inlined via run_slow_path_loop.
use crate::batch::{Measurer, Measurement};
use std::time::Duration;
use tokio::time::interval;

#[allow(dead_code)]
pub async fn start_slow_path<M: Measurer + Clone + 'static>(
    measurer: M,
    mut nodes: Vec<Node>,
    batch_size: usize,
    interval_secs: u64,
    timeout_ms: u64,
    concurrency: usize,
    timeout_penalty: f64,
) {
    let mut ticker = interval(Duration::from_secs(interval_secs));

    loop {
        ticker.tick().await;
        nodes.sort_by(|a, b| a.score().partial_cmp(&b.score()).unwrap());

        let mut start = 0;
        while start < nodes.len() {
            let end = (start + batch_size).min(nodes.len());
            let chunk: Vec<crate::node::Node> = nodes[start..end].to_vec();

            let _results: Vec<Measurement> = crate::batch::run_batch_owned(
                &measurer,
                chunk,
                timeout_ms,
                concurrency,
            )
            .await;

            start = end;
        }

        let _ = timeout_penalty;
    }
}

use crate::node::Node;