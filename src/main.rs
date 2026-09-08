//! lift — 稳定性优先的代理节点调度器（MVP）。
//!
//! 主流程：
//! 1. 从 YAML 加载节点表
//! 2. 用 meow factory 为每个节点构建协议 adapter（复用 meow-rs，零自研协议）
//! 3. 调度循环：分批并发全池测速（ranked 顺序）→ 自适应 EWMA 更新 →
//!    纯 EWMA 选前 N → 写共享选择 + meow SelectorStore
//! 4. 数据面 inbound（SOCKS5/HTTP-CONNECT）读共享选择，经 meow adapter 转发
//!
//! lift 只做调度与转发；协议/传输/TLS/Reality 全部复用 meow-rs。
mod batch;
mod config;
mod decision;
mod fast_path;
mod node;
mod nodespec;

#[cfg(feature = "meow")]
mod factory;
#[cfg(feature = "meow")]
mod inbound;
#[cfg(feature = "meow")]
mod meow;

#[cfg(not(feature = "meow"))]
use batch::NoopMeasurer;

use batch::Measurer;
use config::{
    DEFAULT_ACTIVE_CAPACITY, DEFAULT_BATCH_SIZE, DEFAULT_CONCURRENCY, DEFAULT_SLOW_INTERVAL_SECS,
    DEFAULT_TIMEOUT_MS, DEFAULT_TIMEOUT_PENALTY,
};
use decision::select_top;
use node::Node;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// 共享选择：调度循环写，数据面读（best → 次优 fallback 顺序）。
type SharedSelection = tokio::sync::RwLock<Vec<String>>;
#[cfg(feature = "meow")]
type Registry = meow::Registry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let nodes_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "nodes.yaml".to_string());

    // 1. 加载节点
    let specs = nodespec::load_nodes_yaml(&nodes_path)?;
    tracing::info!(count = specs.len(), "loaded nodes from {nodes_path}");

    // 2. 构建 registry（feature meow）→ measurer + 数据面共用
    #[cfg(feature = "meow")]
    let registry: Registry = {
        let mut reg = std::collections::HashMap::new();
        for spec in &specs {
            match factory::build_proxy(spec) {
                Ok(p) => {
                    reg.insert(spec.tag.clone(), p);
                }
                Err(e) => {
                    tracing::warn!(tag = %spec.tag, "{e} — node skipped");
                }
            }
        }
        Arc::new(parking_lot::RwLock::new(reg))
    };

    let measurer: Arc<dyn Measurer> = {
        #[cfg(feature = "meow")]
        {
            Arc::new(meow::MeowMeasurer::new(registry.clone()))
        }
        #[cfg(not(feature = "meow"))]
        {
            Arc::new(NoopMeasurer)
        }
    };



    // 3. 调度池：Node 只保留 tag（EWMA 载体）
    let nodes: Vec<Node> = specs
        .iter()
        .map(|s| Node::new(s.tag.clone(), s.server.clone(), s.port))
        .collect();
    let pool: Arc<RwLock<Vec<Node>>> = Arc::new(RwLock::new(nodes));
    let selection: Arc<SharedSelection> = Arc::new(SharedSelection::new(Vec::new()));

    // 4. 调度循环（测速 + EWMA + 切换）
    let sched_sel = selection.clone();
    let sched = tokio::spawn(async move {
        schedule_loop(
            measurer,
            pool,
            sched_sel,
            DEFAULT_ACTIVE_CAPACITY,
            DEFAULT_BATCH_SIZE,
            DEFAULT_SLOW_INTERVAL_SECS,
            DEFAULT_TIMEOUT_MS,
            DEFAULT_CONCURRENCY,
            DEFAULT_TIMEOUT_PENALTY,
        )
        .await
    });

    // 5. 数据面 inbound（feature meow）
    #[cfg(feature = "meow")]
    {
        let listen = std::env::var("LIFT_LISTEN").unwrap_or_else(|_| "127.0.0.1:17321".to_string());
        let inbound_sel = selection.clone();
        let inbound_reg = registry.clone();
        let inb = tokio::spawn(async move {
            if let Err(e) = inbound::run(&listen, inbound_reg, inbound_sel).await {
                tracing::error!("inbound exited: {e}");
            }
        });
        tokio::join!(inb, sched);
    }

    #[cfg(not(feature = "meow"))]
    {
        let _ = selection;
        sched.await?;
    }

    Ok(())
}

/// 调度主循环：周期全池测速 → EWMA 更新 → 纯 EWMA 选前 N → 写共享选择。
async fn schedule_loop(
    measurer: Arc<dyn Measurer>,
    pool: Arc<RwLock<Vec<Node>>>,
    selection: Arc<SharedSelection>,
    capacity: usize,
    batch_size: usize,
    interval_secs: u64,
    timeout_ms: u64,
    concurrency: usize,
    timeout_penalty: f64,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    let mut last_selection: Vec<String> = Vec::new();

    loop {
        ticker.tick().await;

        // 快照 + 按当前分数排序（好节点先测）
        let mut snapshot: Vec<Node> = pool.read().await.clone();
        snapshot.sort_by(|a, b| a.score().partial_cmp(&b.score()).unwrap());

        // 分批并发测速（ranked 顺序，不阻塞）
        let mut all_results: Vec<batch::Measurement> = Vec::new();
        for chunk in snapshot.chunks(batch_size) {
            let results =
                batch::run_batch_owned(measurer.as_ref(), chunk.to_vec(), timeout_ms, concurrency)
                    .await;
            all_results.extend(results);
        }

        // 写回 EWMA + 超时扣分
        {
            let mut guard = pool.write().await;
            fast_path::apply_batch(&mut guard, &all_results, timeout_penalty);
        }

        // 纯 EWMA 选前 N → 切换
        let new_selection = select_top(&pool.read().await, capacity);
        if new_selection != last_selection {
            tracing::info!(?new_selection, "selection changed");
            apply_selection(&new_selection, &selection).await;
            last_selection = new_selection;
        }
    }
}

/// 切换执行器：写共享选择（数据面用）+ meow SelectorStore（外部 kernel 用）。
async fn apply_selection(selection: &[String], shared: &Arc<SharedSelection>) {
    *shared.write().await = selection.to_vec();

    #[cfg(feature = "meow")]
    {
        use meow_proxy::group::selector_store::SelectorStore;
        use std::path::PathBuf;
        const GROUP: &str = "lift-active";
        let path = PathBuf::from(
            std::env::var("LIFT_SELECTOR_STORE")
                .unwrap_or_else(|_| {
                    format!(
                        "{}/.local/state/lift-selector.json",
                        std::env::var("HOME").unwrap_or_default()
                    )
                }),
        );
        if let Some(best) = selection.first() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let store = SelectorStore::open(path);
            store.set(GROUP, best);
        }
    }

    #[cfg(not(feature = "meow"))]
    {
        tracing::info!(count = selection.len(), "noop measurer selection (dry-run)");
    }
}