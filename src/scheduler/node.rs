//! Node definition with adaptive EWMA (scheme C: volatility + direction mixture).
//! Alpha is computed automatically based on recent measurements.
//! No manual alpha tuning needed.
use std::collections::VecDeque;
use std::time::Instant;

/// Size of the rolling window used for adaptive alpha calculation.
const EWMA_WINDOW: usize = 7;

/// Bounds for adaptive alpha.
const ALPHA_MIN: f64 = 0.12;
const ALPHA_MAX: f64 = 0.65;

/// 探测成败滚动窗口长度（稳定性/成功率维度，见 `Node::outcomes`）。
const OUTCOME_WINDOW: usize = 16;

/// 每次连续失败在排序上的默认罚分（毫秒等价）。
/// 可经 silverq.toml `[scheduler].timeout_penalty` 覆盖。
pub const DEFAULT_FAILURE_PENALTY_MS: f64 = 3000.0;

/// 每降低 e 倍（≈2.72x）吞吐，在排序上相当于加多少毫秒延迟。
/// 校准依据实测：AD-86（65KB/s）vs BA-1955（265KB/s）≈ 4 倍速差，
/// github 首页 8.9s vs 2.2s ≈ 4 倍耗时差——吞吐差与页面耗时近似线性，
/// 故以 ln 尺度（每 e 倍）配一个固定 ms 权重即可，无需把页面大小硬编码进来。
pub const DEFAULT_BW_PENALTY_PER_EFOLD_MS: f64 = 1500.0;

/// 带宽罚分的「够快」上限：达到该吞吐后带宽项归零，不再被罚。
///
/// **必须是固定基准，不能是池内最优**：`score_with` 没有池上下文，
/// 而且排序只看节点之间的相对差——固定基准对所有节点是同一个常数偏移，
/// 不改变互相顺序，却让「够快」有绝对含义（否则池子整体变慢时罚分
/// 会被基准一起拽低，慢节点白白逃脱）。取 8MB/s：覆盖实测免费节点
/// 上限一个量级，留足头部空间。
pub const BW_REF_BPS: f64 = 8_000_000.0;

#[derive(Debug, Clone)]
#[allow(dead_code)] // server/port 见下方说明
pub struct Node {
    pub tag: String,
    // server/port 目前调度侧不读（测速按 tag 查 registry 里的 adapter），
    // 保留是为了日志可读与将来按节点重建 adapter，不删。
    pub server: String,
    pub port: u16,
    /// 实测延迟的 EWMA（越小越好）。**只由成功测速写入**，绝不掺罚分。
    ///
    /// 早先失败时直接 `ewma += 3000`，把罚分和延迟混在一个字段里，后果：
    /// 1. 面板显示 7512ms，看着像 2500ms 超时失效（实际是 1512ms 真延迟 + 两次罚分）
    /// 2. 罚分是加法、恢复是 alpha 混合（≤0.65），涨得比恢复快 ——
    ///    偶尔失败的活节点被永久压在膨胀分数上，撞到 9999 封顶后
    ///    和真死节点无法区分（实测 samples=77 的活节点显示 9999）
    pub ewma: f64,
    /// 连续失败次数。成功一次即清零。
    ///
    /// 排序惩罚放在 `score()` 里算，不写回 `ewma`，所以恢复是瞬时的：
    /// 一次成功测速立刻回到真实延迟排序，不需要多轮把膨胀分数"洗"回来。
    pub consecutive_failures: u32,
    /// 本轮连续失败的起始时刻（第一次失败时置位，成功即清除）。
    pub failing_since: Option<Instant>,
    pub samples: u32,
    pub last_measured: Option<Instant>,
    /// 延迟采样滚动窗口（用于自适应 alpha）
    recent: VecDeque<f64>,
    /// 延迟历史（面板画图用）：`(unix 秒, 实测延迟 ms)`，只记成功。
    /// 环形缓冲，超长丢最旧。不持久化 —— 重启后图表从零开始攒，可接受。
    pub history: VecDeque<(u64, f64)>,
    /// 带宽 EWMA，**对数域**（ln(bytes/s)）。吞吐是重尾分布，
    /// 线性 EWMA 对 [1MB/s, 10KB/s] 给出 505KB/s（两个都不是），
    /// 对数域给 ~100KB/s，贴合实际体验。
    /// INFINITY = 从未测过带宽（不参与评分，见 `score_with`）。
    pub bw_log: f64,
    /// 带宽采样数。0 = 未测过，score 里带宽项按 0 处理（不奖不罚）。
    pub bw_samples: u32,
    /// 带宽采样滚动窗口（对数值），用于自适应 alpha。
    bw_recent: VecDeque<f64>,
    /// 探测成败滚动窗口（true = 成功）。稳定性/成功率维度：
    /// `consecutive_failures` 一次成功就清零，掩盖了「两次挂一次」的间歇性劣化；
    /// 这个窗口看长期成功率，补上这个盲区。
    outcomes: VecDeque<bool>,
}

/// 每节点保留的延迟历史点数。
pub const HISTORY_CAP: usize = 48;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Node {
    pub fn new(tag: impl Into<String>, server: impl Into<String>, port: u16) -> Self {
        Self {
            tag: tag.into(),
            server: server.into(),
            port,
            ewma: f64::INFINITY,
            bw_log: f64::INFINITY,
            bw_samples: 0,
            bw_recent: VecDeque::with_capacity(EWMA_WINDOW),
            outcomes: VecDeque::with_capacity(OUTCOME_WINDOW),
            consecutive_failures: 0,
            failing_since: None,
            samples: 0,
            last_measured: None,
            recent: VecDeque::with_capacity(EWMA_WINDOW),
            history: VecDeque::with_capacity(HISTORY_CAP),
        }
    }

    /// Update EWMA using scheme C (volatility + direction mixture).
    /// This is the recommended "automatic" update method.
    /// No external alpha is required.
    pub fn update(&mut self, delay_ms: f64) {
        self.consecutive_failures = 0;
        self.failing_since = None;
        self.push_history(delay_ms);
        if self.samples == 0 {
            self.ewma = delay_ms;
            self.recent.push_back(delay_ms);
            self.samples = 1;
            self.last_measured = Some(Instant::now());
            return;
        }

        // 1. Base alpha from coefficient of variation (volatility)
        let (mean, std) = self.window_stats();
        let cv = if mean > 0.0 { std / mean } else { 1.0 };
        let base_alpha = (0.15 + 0.45 * (1.0 - cv)).clamp(ALPHA_MIN, ALPHA_MAX);

        // 2. Direction factor (how many recent samples are on the same side of current EWMA)
        let direction_count: usize = self
            .recent
            .iter()
            .filter(|&&d| (d > self.ewma) == (delay_ms > self.ewma))
            .count();

        let direction_factor = match direction_count {
            0..=1 => 0.75,
            2..=3 => 1.0,
            4..=5 => 1.25,
            _ => 1.4,
        };

        let alpha = (base_alpha * direction_factor).clamp(ALPHA_MIN, ALPHA_MAX);

        // 3. Apply EWMA
        self.ewma = alpha * delay_ms + (1.0 - alpha) * self.ewma;

        // 4. Maintain window
        self.recent.push_back(delay_ms);
        if self.recent.len() > EWMA_WINDOW {
            self.recent.pop_front();
        }

        self.samples += 1;
        self.last_measured = Some(Instant::now());
    }

    fn push_history(&mut self, delay_ms: f64) {
        if self.history.len() >= HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back((unix_now(), delay_ms));
    }

    /// Compute mean and standard deviation of the recent window.
    fn window_stats(&self) -> (f64, f64) {
        if self.recent.is_empty() {
            return (self.ewma, 0.0);
        }
        let n = self.recent.len() as f64;
        let sum: f64 = self.recent.iter().sum();
        let mean = sum / n;
        let variance: f64 = self.recent.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        (mean, variance.sqrt())
    }

    /// 记一次测速失败。只累计次数，不动 `ewma`。
    pub fn penalize(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.failing_since.is_none() {
            self.failing_since = Some(Instant::now());
        }
        self.last_measured = Some(Instant::now());
    }

    /// 记一次探测成败到稳定性窗口。成功失败都记：
    /// 只记失败会把「两次挂一次」的间歇性劣化算成完全健康。
    pub fn record_outcome(&mut self, ok: bool) {
        self.outcomes.push_back(ok);
        if self.outcomes.len() > OUTCOME_WINDOW {
            self.outcomes.pop_front();
        }
    }

    /// 稳定性乘子（1.0 = 完全稳定，越小越差）。窗口为空返回 1.0：
    /// 无数据不惩罚，新节点和重启后的节点不该被假设成不可靠。
    pub fn stability(&self) -> f64 {
        if self.outcomes.is_empty() {
            return 1.0;
        }
        let wins = self.outcomes.iter().filter(|&&o| o).count() as f64;
        wins / self.outcomes.len() as f64
    }

    /// 更新带宽 EWMA（**对数域**）。`bps` = bytes/sec，必须 > 0。
    ///
    /// 复用延迟侧的自适应 alpha（变异系数 + 方向因子）：吞吐波动同样是
    /// 「该快追还是该稳」的问题，无需另造一套。输入取对数后 EWMA 的语义
    /// 从算术平均变成几何平均，对重尾分布正确。
    pub fn update_bw(&mut self, bps: f64) {
        if !(bps > 0.0) {
            return; // bps <= 0 对数域无定义，静默丢弃
        }
        let x = bps.ln();
        if self.bw_samples == 0 {
            self.bw_log = x;
            self.bw_recent.push_back(x);
            self.bw_samples = 1;
            return;
        }
        let (mean, std) = self.bw_window_stats();
        let cv = if mean.abs() > 0.0 {
            std / mean.abs()
        } else {
            1.0
        };
        let base_alpha = (0.15 + 0.45 * (1.0 - cv)).clamp(ALPHA_MIN, ALPHA_MAX);
        let direction_count = self
            .bw_recent
            .iter()
            .filter(|&&d| (d > self.bw_log) == (x > self.bw_log))
            .count();
        let direction_factor = match direction_count {
            0..=1 => 0.75,
            2..=3 => 1.0,
            4..=5 => 1.25,
            _ => 1.4,
        };
        let alpha = (base_alpha * direction_factor).clamp(ALPHA_MIN, ALPHA_MAX);
        self.bw_log = alpha * x + (1.0 - alpha) * self.bw_log;
        self.bw_recent.push_back(x);
        if self.bw_recent.len() > EWMA_WINDOW {
            self.bw_recent.pop_front();
        }
        self.bw_samples += 1;
    }

    /// 带宽（bytes/sec）。未测过返回 None。
    pub fn bw_bps(&self) -> Option<f64> {
        if self.bw_samples > 0 && self.bw_log.is_finite() {
            Some(self.bw_log.exp())
        } else {
            None
        }
    }

    fn bw_window_stats(&self) -> (f64, f64) {
        if self.bw_recent.is_empty() {
            return (self.bw_log, 0.0);
        }
        let n = self.bw_recent.len() as f64;
        let sum: f64 = self.bw_recent.iter().sum();
        let mean = sum / n;
        let variance: f64 = self
            .bw_recent
            .iter()
            .map(|&x| (x - mean).powi(2))
            .sum::<f64>()
            / n;
        (mean, variance.sqrt())
    }

    /// 便捷入口：用内置默认罚分。面板排序与 ctl 路径用这个。
    #[cfg_attr(not(feature = "meow"), allow(dead_code))] // 排序入口在 meow 侧
    pub fn score(&self) -> f64 {
        self.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS)
    }

    /// 复合排序分数（越小越好）：
    /// ```text
    /// score = (延迟 + 连续失败罚分) / 稳定性  +  带宽罚分
    /// ```
    /// - **稳定性做分母（乘性）**：半死节点的有效代价按 1/成功率 放大
    ///   （50% 成功率 → 分数翻倍）。加性罚分做不到成比例：
    ///   固定加 300ms 对一个 100ms 的快节点和一个 2000ms 的慢节点
    ///   压制力完全不同，乘性才量纲一致。
    /// - **带宽在对数尺度上相加**：吞吐与延迟量纲不同，直接相加会有一方
    ///   淹没另一方；归一化为「每 e 倍吞吐差 = N ms 延迟」后量纲统一。
    /// - **未测过带宽的节点带宽项为 0**（不奖不罚）：首轮靠延迟+稳定性
    ///   排序，下一轮带宽数据进来再修正——乐观初值，SW-UCB 的探索精神。
    /// 同 `score()`，罚分幅度可指定（配置里的 `timeout_penalty` 与 `bw_penalty_per_efold_ms`）。
    pub fn score_with(&self, penalty_ms: f64, bw_penalty_per_efold_ms: f64) -> f64 {
        let base = if self.consecutive_failures == 0 {
            self.ewma
        } else {
            self.ewma + self.consecutive_failures as f64 * penalty_ms
        };
        // 稳定性为 0（窗口内全失败）时除零 → 地板兜住，保证分数有限可排序。
        let stab = self.stability().max(0.05);
        let mut score = base / stab;
        if self.bw_samples > 0 && self.bw_log.is_finite() {
            // 对数域落差：比 BW_REF_BPS 慢多少个 e 倍，每个 e 倍罚
            // bw_penalty_per_efold_ms 毫秒。线性差在重尾分布下毫无意义
            // （1MB/s vs 10KB/s 差 999990，彻底淹没延迟项）；
            // 对数差单调、量纲统一，排序够用。
            let gap = (BW_REF_BPS.ln() - self.bw_log).max(0.0);
            score += gap * bw_penalty_per_efold_ms;
        }
        score
    }

    /// 从存档恢复分数。`recent` 窗口不恢复（只影响自适应 alpha 的头几次取值，
    /// 不影响排序），所以重启后 alpha 会先偏保守，几次测量后回归正常。
    pub fn restore_score(&mut self, ewma: f64, samples: u32) {
        self.ewma = ewma;
        self.samples = samples;
    }

    /// 从存档恢复带宽分数。`bw_recent` 窗口不恢复（同延迟侧的道理：
    /// 只影响头几次自适应 alpha，不影响排序）。
    pub fn restore_bw(&mut self, bw_log: f64, bw_samples: u32) {
        if bw_log.is_finite() && bw_samples > 0 {
            self.bw_log = bw_log;
            self.bw_samples = bw_samples;
        }
    }

    /// 从另一个 Node 接管 EWMA 状态（配置热加载时保留分数）。
    ///
    /// `ewma` 必须接管：`select_top` 现在按 `ewma.is_finite()` 过滤节点，
    /// reload 后新 Node 全是 INFINITY —— 不接管 = 整池被滤空 = selection
    /// 清空 = 数据面无候选可拨（reload 一度把代理打成纯直连的回归）。
    pub fn adopt_score(&mut self, other: &Node) {
        self.ewma = other.ewma;
        self.consecutive_failures = other.consecutive_failures;
        self.failing_since = other.failing_since;
        self.samples = other.samples;
        self.last_measured = other.last_measured;
        self.bw_log = other.bw_log;
        self.bw_samples = other.bw_samples;
        self.bw_recent = other.bw_recent.clone();
        self.outcomes = other.outcomes.clone();
    }
}
