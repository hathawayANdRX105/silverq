//! lift - stability-first proxy node scheduler (final runnable version).
//!
//! Uses MeowMeasurer (feature "meow") to delegate all protocol/transport
//! concerns to meow-rs. lift only handles measurement batching, adaptive
//! EWMA scoring, and node selection.
mod batch;
mod config;
mod decision;
mod fast_path;
mod node;
mod slow_path;

#[cfg(feature = "meow")]
mod meow;

#[cfg(feature = "meow")]
use crate::meow::MeowMeasurer;

#[cfg(not(feature = "meow"))]
use crate::batch::NoopMeasurer;

use crate::batch::Measurer;
use crate::config::{
    DEFAULT_ACTIVE_CAPACITY, DEFAULT_BATCH_SIZE, DEFAULT_CONCURRENCY, DEFAULT_SLOW_INTERVAL_SECS,
    DEFAULT_TIMEOUT_MS, DEFAULT_TIMEOUT_PENALTY,
};
use crate::decision::select_top;
use crate::node::Node;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let initial_nodes: Vec<Node> = (0..20)
        .map(|i| Node::new(format!("node-{}", i), "127.0.0.1", 443))
        .collect();

    let nodes = Arc::new(RwLock::new(initial_nodes.clone()));
    tracing::info!("lift starting with {} nodes", initial_nodes.len());

    // Choose measurer based on feature.
    // meow: real protocol-aware delay via meow-rs
    // default: NoopMeasurer (testing)
    let measurer: Arc<dyn Measurer> = {
        #[cfg(feature = "meow")]
        {
            Arc::new(MeowMeasurer::new())
        }
        #[cfg(not(feature = "meow"))]
        {
            Arc::new(NoopMeasurer)
        }
    };

    // Slow path: periodic full-pool measurement in background.
    let slow_nodes = nodes.clone();
    let slow_measurer: Arc<dyn Measurer> = measurer.clone();
    let slow_handle = tokio::spawn(async move {
        run_slow_path_loop(
            slow_measurer,
            slow_nodes,
            DEFAULT_BATCH_SIZE,
            DEFAULT_SLOW_INTERVAL_SECS,
            DEFAULT_TIMEOUT_MS,
            DEFAULT_CONCURRENCY,
            DEFAULT_TIMEOUT_PENALTY,
        )
        .await;
    });

    // Demo loop: every 10s read current EWMA and select top active set.
    for _ in 0..5 {
        sleep(Duration::from_secs(10)).await;

        let snapshot = nodes.read().await;
        let top = select_top(&snapshot, DEFAULT_ACTIVE_CAPACITY);

        tracing::info!("current active set (top by EWMA): {:?}", top);
    }

    let _ = slow_handle.await;
}

/// Slow-path loop that uses shared Arc<RwLock<Vec<Node>>>.
/// Each cycle sorts the pool by current EWMA, processes in ranked batches,
/// applies measurements, and updates scores in place.
async fn run_slow_path_loop(
    measurer: Arc<dyn Measurer>,
    nodes: Arc<RwLock<Vec<Node>>>,
    batch_size: usize,
    interval_secs: u64,
    timeout_ms: u64,
    concurrency: usize,
    timeout_penalty: f64,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        ticker.tick().await;

        // 1. Snapshot and rank by current score (best first)
        let mut snapshot: Vec<Node> = {
            let guard = nodes.read().await;
            guard.clone()
        };
        snapshot.sort_by(|a, b| a.score().partial_cmp(&b.score()).unwrap());

        // 2. Process in ranked batches without blocking the read lock.
        let mut all_results: Vec<crate::batch::Measurement> = Vec::new();
        for chunk in snapshot.chunks(batch_size) {
            let results =
                crate::batch::run_batch_owned(measurer.as_ref(), chunk.to_vec(), timeout_ms, concurrency).await;
            for r in results {
                all_results.push(r);
            }
        }

        // 3. Apply results to the shared pool
        {
            let mut guard = nodes.write().await;
            crate::fast_path::apply_batch(&mut guard, &all_results, timeout_penalty);
        }

        tracing::info!("slow_path: cycle complete ({} nodes)", snapshot.len());
    }
}