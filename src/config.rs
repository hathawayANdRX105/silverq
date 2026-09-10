//! 默认参数与环境变量覆盖。
//!
//! 取值取向（按需求）：
//! - 分批按当前排名顺序（好节点先测）
//! - 超时节点大幅扣分，排到后排观察
//!
//! 环境变量覆盖存在的理由：e2e 测试要能指向本地探测端点、把调度间隔压到 1s，
//! 否则测试要么依赖外网、要么等 30s。生产不设这些变量就用下面的默认值。
pub const DEFAULT_BATCH_SIZE: usize = 5;
pub const DEFAULT_CONCURRENCY: usize = 6;
pub const DEFAULT_TIMEOUT_MS: u64 = 2000;
pub const DEFAULT_TIMEOUT_PENALTY: f64 = 3000.0;
pub const DEFAULT_SLOW_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_ACTIVE_CAPACITY: usize = 5;
pub const DEFAULT_LISTEN: &str = "127.0.0.1:17321";

/// gstatic generate_204：无 body、稳定，适合做延迟探测。
/// 仅 meow feature 下用到（真实探测），默认模式的 NoopMeasurer 不发请求。
#[cfg_attr(not(feature = "meow"), allow(dead_code))]
pub const DEFAULT_PROBE_URL: &str = "https://www.gstatic.com/generate_204";

fn env_parsed<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 探测 URL（`SILVERQ_PROBE_URL`）。仅 meow feature 下调用。
#[cfg_attr(not(feature = "meow"), allow(dead_code))]
pub fn probe_url() -> String {
    std::env::var("SILVERQ_PROBE_URL").unwrap_or_else(|_| DEFAULT_PROBE_URL.to_string())
}

/// fallback 尝试上限（`SILVERQ_FALLBACK_ATTEMPTS`）。
#[cfg_attr(feature = "meow", allow(dead_code))] // 生产走 settings::Effective
#[cfg_attr(not(feature = "meow"), allow(dead_code))]
pub fn fallback_attempts() -> usize {
    env_parsed("SILVERQ_FALLBACK_ATTEMPTS", 3)
}
