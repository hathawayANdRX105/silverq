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

/// 每次连续失败在排序上的默认罚分（毫秒等价）。
/// 可经 silverq.toml `[scheduler].timeout_penalty` 覆盖。
pub const DEFAULT_FAILURE_PENALTY_MS: f64 = 3000.0;

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
    /// Number of samples seen
    pub samples: u32,
    /// Last measurement timestamp
    pub last_measured: Option<Instant>,
    /// Rolling window of recent delay measurements (for adaptive alpha)
    recent: VecDeque<f64>,
}

impl Node {
    pub fn new(tag: impl Into<String>, server: impl Into<String>, port: u16) -> Self {
        Self {
            tag: tag.into(),
            server: server.into(),
            port,
            ewma: f64::INFINITY,
            consecutive_failures: 0,
            samples: 0,
            last_measured: None,
            recent: VecDeque::with_capacity(EWMA_WINDOW),
        }
    }

    /// Update EWMA using scheme C (volatility + direction mixture).
    /// This is the recommended "automatic" update method.
    /// No external alpha is required.
    pub fn update(&mut self, delay_ms: f64) {
        self.consecutive_failures = 0;

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
        self.last_measured = Some(Instant::now());
    }

    /// 排序分数（越小越好）= 实测延迟 + 连续失败罚分。
    ///
    /// 罚分在这里现算而不写回 `ewma`：一次成功就清零 `consecutive_failures`，
    /// 排序立刻回到真实延迟。早先把罚分累加进 `ewma`，恢复要靠 alpha 混合
    /// 慢慢洗，罚分涨得比恢复快，活节点会被永久压住。
    ///
    /// 从未测通过的节点 `ewma` 是 INFINITY，加什么都还是 INFINITY，天然排最后。
    #[cfg_attr(not(feature = "meow"), allow(dead_code))] // 排序入口在 meow 侧
    pub fn score(&self) -> f64 {
        self.score_with(DEFAULT_FAILURE_PENALTY_MS)
    }

    /// 同 `score()`，罚分幅度可指定（配置里的 `timeout_penalty`）。
    pub fn score_with(&self, penalty_ms: f64) -> f64 {
        if self.consecutive_failures == 0 {
            return self.ewma;
        }
        self.ewma + self.consecutive_failures as f64 * penalty_ms
    }

    /// 从存档恢复分数。`recent` 窗口不恢复（只影响自适应 alpha 的头几次取值，
    /// 不影响排序），所以重启后 alpha 会先偏保守，几次测量后回归正常。
    pub fn restore_score(&mut self, ewma: f64, samples: u32) {
        self.ewma = ewma;
        self.samples = samples;
    }

    /// 从另一个 Node 接管 EWMA 状态（配置热加载时保留分数）。
    pub fn adopt_score(&mut self, other: &Node) {
        self.ewma = other.ewma;
        self.consecutive_failures = other.consecutive_failures;
        self.samples = other.samples;
        self.last_measured = other.last_measured;
        self.recent = other.recent.clone();
    }
}
