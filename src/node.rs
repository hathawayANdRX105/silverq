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

#[derive(Debug, Clone)]
#[allow(dead_code)] // server/port 见下方说明
pub struct Node {
    pub tag: String,
    // server/port 目前调度侧不读（测速按 tag 查 registry 里的 adapter），
    // 保留是为了日志可读与将来按节点重建 adapter，不删。
    pub server: String,
    pub port: u16,
    /// Current EWMA score (lower is better)
    pub ewma: f64,
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
            samples: 0,
            last_measured: None,
            recent: VecDeque::with_capacity(EWMA_WINDOW),
        }
    }

    /// Update EWMA using scheme C (volatility + direction mixture).
    /// This is the recommended "automatic" update method.
    /// No external alpha is required.
    pub fn update(&mut self, delay_ms: f64) {
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

    /// Stability score (lower is better)
    pub fn score(&self) -> f64 {
        self.ewma
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
        self.samples = other.samples;
        self.last_measured = other.last_measured;
        self.recent = other.recent.clone();
    }
}
