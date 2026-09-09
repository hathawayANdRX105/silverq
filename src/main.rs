//! lift — 稳定性优先的代理节点调度器（MVP）。
//!
//! 子命令：
//!   lift serve [nodes.yaml]     启动 daemon（调度 + 数据面 + ctl socket）
//!   lift reload [nodes.yaml]    热加载节点表（存活节点继承 EWMA）
//!   lift select <tag|auto>      手动钉住 / 取消钉住
//!   lift status                 查看当前状态
//!
//! 协议/传输/TLS/Reality/QUIC 全部复用 meow-rs；lift 只做调度与转发。
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
/// TUN 数据面未实现，见模块文档；不接线，仅作占位提醒。
#[cfg(feature = "meow")]
mod tun;
#[cfg(feature = "meow")]
mod udp;

#[cfg(unix)]
mod cli;
#[cfg(unix)]
mod ctl;

#[cfg(not(feature = "meow"))]
use batch::NoopMeasurer;

use batch::Measurer;
use config::{DEFAULT_BATCH_SIZE, DEFAULT_CONCURRENCY, DEFAULT_TIMEOUT_PENALTY};
use node::Node;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// 共享选择：调度循环写，数据面读（best → 次优 fallback 顺序）。
type SharedSelection = Arc<RwLock<Vec<String>>>;
#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cmd = cli::parse();
    match &cmd {
        cli::Cmd::Serve { nodes } => serve(nodes.clone()).await,
        _ => {
            ctl::client(&cli::ctl_line(&cmd)).await?;
            Ok(())
        }
    }
}

/// 非 unix 平台（Android/iOS FFI 场景）无 ctl socket，直接 serve。
#[cfg(not(unix))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    serve("nodes.yaml".into()).await
}

async fn serve(nodes: String) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 加载节点
    let specs = nodespec::load_nodes_yaml(&nodes)?;
    tracing::info!(count = specs.len(), "loaded nodes from {nodes}");

    // 2. registry（feature meow）→ measurer + 数据面 + ctl 共用
    #[cfg(feature = "meow")]
    let registry = {
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
    let nodes_pool: Vec<Node> = specs
        .iter()
        .map(|s| Node::new(s.tag.clone(), s.server.clone(), s.port))
        .collect();
    let pool: Arc<RwLock<Vec<Node>>> = Arc::new(RwLock::new(nodes_pool));
    let selection: SharedSelection = Arc::new(RwLock::new(Vec::new()));
    let pinned = Arc::new(AtomicBool::new(false));

    // 4. 调度循环（测速 + EWMA + 切换），pinned 时暂停覆盖
    let handles = SchedulerHandles {
        measurer: measurer.clone(),
        pool: pool.clone(),
        selection: selection.clone(),
        pinned: pinned.clone(),
    };
    let sched_cfg = SchedulerConfig {
        capacity: config::active_capacity(),
        batch_size: DEFAULT_BATCH_SIZE,
        interval_secs: config::interval_secs(),
        timeout_ms: config::timeout_ms(),
        concurrency: DEFAULT_CONCURRENCY,
        timeout_penalty: DEFAULT_TIMEOUT_PENALTY,
    };
    let sched = tokio::spawn(schedule_loop(handles, sched_cfg));

    // 5. ctl socket（unix）：reload / select / status
    #[cfg(unix)]
    {
        #[cfg(feature = "meow")]
        let ctl_state = Arc::new(ctl::CtlState::new(
            registry.clone(),
            pool.clone(),
            selection.clone(),
            nodes.clone(),
        ));
        #[cfg(not(feature = "meow"))]
        let ctl_state = Arc::new(ctl::CtlState::new(
            pool.clone(),
            selection.clone(),
            nodes.clone(),
        ));
        let inb = tokio::spawn(async move {
            if let Err(e) = ctl::serve_ctl(ctl_state).await {
                tracing::error!("ctl socket exited: {e}");
            }
        });

        // 6. 数据面 inbound（feature meow）
        #[cfg(feature = "meow")]
        {
            let listen =
                std::env::var("LIFT_LISTEN").unwrap_or_else(|_| "127.0.0.1:17321".to_string());
            let inbound_sel = selection.clone();
            let inbound_reg = registry.clone();
            let inb2 = tokio::spawn(async move {
                if let Err(e) = inbound::run(&listen, inbound_reg, inbound_sel).await {
                    tracing::error!("inbound exited: {e}");
                }
            });
            // 三个任务都是常驻的；任一退出都说明出事了。JoinError 必须报出来，
            // 否则 panic 被静默吞掉，进程看起来还活着但实际已经瘸了。
            let (ctl_res, inbound_res, sched_res) = tokio::join!(inb, inb2, sched);
            report_task_exit("ctl", ctl_res);
            report_task_exit("inbound", inbound_res);
            report_task_exit("scheduler", sched_res);
        }
        #[cfg(not(feature = "meow"))]
        {
            let (ctl_res, sched_res) = tokio::join!(inb, sched);
            report_task_exit("ctl", ctl_res);
            report_task_exit("scheduler", sched_res);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (&measurer, &pool, &selection, &pinned);
        sched.await?;
    }

    Ok(())
}

/// 常驻任务退出时把原因报出来（含 panic）。
/// 静默 `let _ = join!(...)` 会让 panic 消失，进程看着还在跑，实则功能已缺。
fn report_task_exit(name: &str, res: Result<(), tokio::task::JoinError>) {
    match res {
        Ok(()) => tracing::warn!(task = name, "常驻任务提前退出"),
        Err(e) if e.is_panic() => tracing::error!(task = name, "任务 panic: {e}"),
        Err(e) => tracing::warn!(task = name, "任务异常结束: {e}"),
    }
}

/// 调度参数。收拢成结构体：之前 10 个位置参数，加一个就得改所有调用点，
/// 且相邻的同类型 usize/u64 很容易传错位置。
struct SchedulerConfig {
    capacity: usize,
    batch_size: usize,
    interval_secs: u64,
    timeout_ms: u64,
    concurrency: usize,
    timeout_penalty: f64,
}

/// 调度共享句柄。
struct SchedulerHandles {
    measurer: Arc<dyn Measurer>,
    pool: Arc<RwLock<Vec<Node>>>,
    selection: SharedSelection,
    /// true 时跳过切换（手动钉住中），但测速继续
    pinned: Arc<AtomicBool>,
}

/// 调度主循环：周期全池测速 → EWMA 更新 → 纯 EWMA 选前 N → 写共享选择。
async fn schedule_loop(h: SchedulerHandles, cfg: SchedulerConfig) {
    let SchedulerHandles {
        measurer,
        pool,
        selection,
        pinned,
    } = h;
    let SchedulerConfig {
        capacity,
        batch_size,
        interval_secs,
        timeout_ms,
        concurrency,
        timeout_penalty,
    } = cfg;
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

        // 写回 EWMA + 超时扣分（钉住时仍更新分数，只暂停切换）
        {
            let mut guard = pool.write().await;
            fast_path::apply_batch(&mut guard, &all_results, timeout_penalty);
        }

        // 纯 EWMA 选前 N → 切换（pinned 时暂停）
        if !pinned.load(Ordering::Relaxed) {
            let new_selection = decision::select_top(&pool.read().await, capacity);
            if new_selection != last_selection {
                tracing::info!(?new_selection, "selection changed");
                apply_selection(&new_selection, &selection).await;
                last_selection = new_selection;
            }
        }
    }
}

/// 切换执行器：写共享选择（数据面用）+ meow SelectorStore（外部 kernel 用）。
async fn apply_selection(selection: &[String], shared: &SharedSelection) {
    *shared.write().await = selection.to_vec();

    #[cfg(feature = "meow")]
    {
        use meow_proxy::group::selector_store::SelectorStore;
        use std::path::PathBuf;
        const GROUP: &str = "lift-active";
        let path = PathBuf::from(std::env::var("LIFT_SELECTOR_STORE").unwrap_or_else(|_| {
            format!(
                "{}/.local/state/lift-selector.json",
                std::env::var("HOME").unwrap_or_default()
            )
        }));
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
