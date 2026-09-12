//! 调度域：节点池、自适应 EWMA、分批测速、选择与持久化。
pub mod batch;
pub mod decision;
pub mod fast_path;
pub mod node;
pub mod persist;

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// 调度进度快照：schedule_loop 写（全部原子，无锁），web 面板读。
/// 只含"面板想看"的计数，调度内部状态不进这里。
#[derive(Default)]
pub struct SchedulerProgress {
    /// 第几轮测速（从 1 起）
    pub round: AtomicU64,
    /// 本轮总批数
    pub round_batches: AtomicU64,
    /// 本轮已完成批数
    pub batches_done: AtomicU64,
    /// 最近一批完成的 unix 秒（面板算"多久前测过"）
    pub last_batch_ts: AtomicI64,
    /// 上一整轮耗时（秒）
    pub last_round_secs: AtomicU64,
}

impl SchedulerProgress {
    pub fn round_begin(&self, total_batches: usize) {
        self.round.fetch_add(1, Ordering::Relaxed);
        self.round_batches
            .store(total_batches as u64, Ordering::Relaxed);
        self.batches_done.store(0, Ordering::Relaxed);
    }

    pub fn batch_done(&self, now_unix: i64) {
        self.batches_done.fetch_add(1, Ordering::Relaxed);
        self.last_batch_ts.store(now_unix, Ordering::Relaxed);
    }

    pub fn round_end(&self, secs: u64) {
        self.last_round_secs.store(secs, Ordering::Relaxed);
    }
}
