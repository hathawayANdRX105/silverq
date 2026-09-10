//! silverq — 稳定性优先的代理节点调度器（MVP）。
//!
//! 子命令：
//!   silverq serve [nodes.yaml]     启动 daemon（调度 + 数据面 + ctl socket）
//!   silverq reload [nodes.yaml]    热加载节点表（存活节点继承 EWMA）
//!   silverq select <tag|auto>      手动钉住 / 取消钉住
//!   silverq status                 查看当前状态
//!
//! 协议/传输/TLS/Reality/QUIC 全部复用 meow-rs；silverq 只做调度与转发。
mod batch;
mod config;
mod decision;
mod fast_path;
mod node;
mod nodespec;
mod persist;
mod settings;

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
#[cfg(feature = "meow")]
mod web;

#[cfg(unix)]
mod cli;
#[cfg(unix)]
mod ctl;

#[cfg(not(feature = "meow"))]
use batch::NoopMeasurer;

use batch::Measurer;
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
        cli::Cmd::Serve { nodes, config } => serve(nodes.clone(), config.clone()).await,
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

async fn serve(nodes: String, cfg_path: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    // 0. 配置：TOML 为底，env 覆盖
    let cfg_path = cfg_path.unwrap_or_else(|| "silverq.toml".into());
    let file_cfg = settings::load(std::path::Path::new(&cfg_path))?;
    let eff = settings::Effective::from(&file_cfg);
    tracing::info!(
        config = %cfg_path,
        listen = %eff.listen,
        fallback_attempts = eff.fallback_attempts,
        interval_secs = eff.interval_secs,
        "配置加载完成"
    );

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
            Arc::new(meow::MeowMeasurer::with_probe(
                registry.clone(),
                eff.probe_url.clone(),
                eff.timeout_ms,
            ))
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
    let mut nodes_pool = nodes_pool;
    let restored = persist::load_from(std::path::Path::new(&eff.state), &mut nodes_pool);
    if restored > 0 {
        tracing::info!(restored, "从存档恢复 EWMA 分数");
    }
    let pool: Arc<RwLock<Vec<Node>>> = Arc::new(RwLock::new(nodes_pool));
    let selection: SharedSelection = Arc::new(RwLock::new(Vec::new()));
    let pinned = Arc::new(AtomicBool::new(false));
    let pin_target: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));

    // 4. 调度循环（测速 + EWMA + 切换），pinned 时暂停覆盖
    let handles = SchedulerHandles {
        measurer: measurer.clone(),
        pool: pool.clone(),
        selection: selection.clone(),
        pinned: pinned.clone(),
        pin_target: pin_target.clone(),
        selector_store: eff.selector_store.clone(),
    };
    let sched_cfg = SchedulerConfig {
        capacity: eff.capacity,
        batch_size: eff.batch_size,
        interval_secs: eff.interval_secs,
        timeout_ms: eff.timeout_ms,
        concurrency: eff.concurrency,
        timeout_penalty: eff.timeout_penalty,
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
            pinned.clone(),
            pin_target.clone(),
        ));
        #[cfg(not(feature = "meow"))]
        let ctl_state = Arc::new(ctl::CtlState::new(
            pool.clone(),
            selection.clone(),
            nodes.clone(),
            pinned.clone(),
            pin_target.clone(),
        ));
        let ctl_sock = std::path::PathBuf::from(eff.ctl_sock.clone());
        #[cfg(feature = "meow")]
        let web_state = ctl_state.clone();
        let inb = tokio::spawn(async move {
            if let Err(e) = ctl::serve_ctl(ctl_state, ctl_sock).await {
                tracing::error!("ctl socket exited: {e}");
            }
        });
        // Web 面板（默认 127.0.0.1:9095；SILVERQ_WEB_LISTEN / TOML [data_plane].web_listen 覆盖）
        #[cfg(feature = "meow")]
        {
            let web_addr = eff.web_listen.clone();
            tokio::spawn(async move {
                if let Err(e) = web::run(&web_addr, web_state).await {
                    tracing::error!("web dashboard exited: {e}");
                }
            });
        }
        // 6. 数据面 inbound（feature meow）
        #[cfg(feature = "meow")]
        {
            let listen = eff.listen.clone();
            let inbound_sel = selection.clone();
            let inbound_reg = registry.clone();
            let inb2 = tokio::spawn(async move {
                if let Err(e) =
                    inbound::run(&listen, inbound_reg, inbound_sel, eff.fallback_attempts).await
                {
                    tracing::error!("inbound exited: {e}");
                }
            });
            // 常驻任务任一退出都说明出事了。JoinError 必须报出来，
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
        let _ = (&measurer, &pool, &selection, &pinned, &eff);
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
    /// 钉住目标：Some 时调度循环把 selection 强制设为它（单写者）
    pin_target: Arc<parking_lot::Mutex<Option<String>>>,
    /// SelectorStore 路径（publish_selector_store 用）
    selector_store: String,
}

/// 调度主循环：周期全池测速 → EWMA 更新 → 纯 EWMA 选前 N → 写共享选择。
///
/// # 冷启动
///
/// 真实节点池（179 个 / 6 并发 / 2.5s 超时）跑完一轮要约 75s。早先的实现把
/// `ticker.tick()` 放在循环开头、且只在整轮结束后才写 selection，导致启动后
/// **85 秒内 selection 为空、数据面所有请求返回失败**（实测 http_code=000）。
///
/// 三处修正：
/// 1. 启动即用配置顺序播种 selection —— 未测速时按配置序当候选，inbound 的
///    best→次优 fallback 会自然跳过死节点，比"什么都没有"强得多
/// 2. 首个 tick 立刻触发（`MissedTickBehavior` 默认首 tick 是立即的，
///    但要先 tick 再算间隔，不能先干等一个 interval）
/// 3. **每批测完就发布一次**中间结果，而不是等整轮 —— 头几批就是当前最优节点，
///    第一批（6 个）测完约 2.5s 内数据面就可用
async fn schedule_loop(h: SchedulerHandles, cfg: SchedulerConfig) {
    let SchedulerHandles {
        measurer,
        pool,
        selection,
        pinned,
        pin_target,
        selector_store,
    } = h;
    let SchedulerConfig {
        capacity,
        batch_size,
        interval_secs,
        timeout_ms,
        concurrency,
        timeout_penalty,
    } = cfg;

    // 冷启动播种：还没有任何测速数据时，按配置顺序给数据面一个候选集，
    // 免得首轮测完前（真实池约 75s）所有请求直接失败。
    if selection.read().await.is_empty() && !pinned.load(Ordering::Relaxed) {
        let seed: Vec<String> = pool
            .read()
            .await
            .iter()
            .take(capacity)
            .map(|n| n.tag.clone())
            .collect();
        if !seed.is_empty() {
            tracing::info!(count = seed.len(), "冷启动：按配置顺序播种 selection");
            *selection.write().await = seed;
        }
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    let mut last_selection: Vec<String> = Vec::new();

    loop {
        ticker.tick().await;

        // 交错分批：已测的按分数、未测的按配置序，每批混合两者。
        // 详见 decision::measurement_order 的文档（含为什么必须交错）。
        let batches = {
            let guard = pool.read().await;
            decision::measurement_order(&guard, batch_size)
        };

        for chunk in batches {
            let results =
                batch::run_batch_owned(measurer.as_ref(), chunk, timeout_ms, concurrency).await;

            {
                let mut guard = pool.write().await;
                fast_path::apply_batch(&mut guard, &results, timeout_penalty);
            }

            // 切换（pinned 时暂停）。
            //
            // 「算 selection」和「写 selection」必须在**同一个写锁临界区**内：
            // ctl/web 的 select 是「置 pinned → 写 selection」两步，如果这里先在
            // 读锁里算完、再去拿写锁，中间那个窗口足够让 select 插进来，
            // 随后本批的旧结果就把刚钉住的节点覆盖掉。
            // 实测：先缩小窗口（写锁内复查 pinned）仍有 2/5 概率被盖回 10 个候选；
            // 只有把计算也纳入临界区才彻底消除。
            {
                // selection 的唯一写者。pinned 时强制为 pin_target，
                // 否则按 EWMA 选前 N。单写者消除了与 ctl/web 的双写竞争。
                let target = pin_target.lock().clone();
                let desired = match (&target, pinned.load(Ordering::Relaxed)) {
                    (Some(tag), true) => vec![tag.clone()],
                    _ => decision::select_top(&pool.read().await, capacity),
                };
                let mut sel_guard = selection.write().await;
                if *sel_guard != desired {
                    *sel_guard = desired.clone();
                    drop(sel_guard);
                    publish_selector_store(&desired, &selector_store);
                }
                last_selection = desired;
            }

            persist::save(&pool.read().await);
        }

        tracing::info!(
            selection = ?last_selection,
            "测速轮完成"
        );
    }
}

/// 切换执行器：写共享选择（数据面用）+ meow SelectorStore（外部 kernel 用）。
///
/// 现在按批调用（217 节点 ≈ 44 批/轮），所以不能每次 `SelectorStore::open`
/// ——那会每批读一次盘、建一个新 Arc。meow 的 store 自带进程级全局槽，
/// 首次 open 后用 `global()` 复用；`store.set` 内部对同值写入是 no-op，
/// 所以 best 没变时不落盘。
/// 只写 meow SelectorStore（外部 kernel 读），不碰共享 selection。
///
/// 与写共享 selection 分开，是因为写 selection 必须在 pinned 复查的同一个
/// 临界区里完成（见 schedule_loop），而 store 写入不需要持锁。
#[cfg_attr(not(feature = "meow"), allow(unused_variables))]
fn publish_selector_store(selection: &[String], selector_store: &str) {
    #[cfg(feature = "meow")]
    {
        use meow_proxy::group::selector_store::SelectorStore;
        const GROUP: &str = "silverq-active";

        let Some(best) = selection.first() else {
            return;
        };
        let store = match SelectorStore::global() {
            Some(s) => s,
            None => {
                let path = std::path::PathBuf::from(selector_store);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                SelectorStore::open(path)
            }
        };
        store.set(GROUP, best);
    }

    #[cfg(not(feature = "meow"))]
    {
        tracing::info!(count = selection.len(), "noop measurer selection (dry-run)");
    }
}
