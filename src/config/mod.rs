//! 默认参数与环境变量覆盖。
//!
//! 取值取向（按需求）：
//! - 分批按当前排名顺序（好节点先测）
//! - 超时节点大幅扣分，排到后排观察
//!
//! 环境变量覆盖存在的理由：e2e 测试要能指向本地探测端点、把调度间隔压到 1s，
//! 否则测试要么依赖外网、要么等 30s。生产不设这些变量就用下面的默认值。
pub mod settings;

pub const DEFAULT_BATCH_SIZE: usize = 5;
pub const DEFAULT_CONCURRENCY: usize = 6;
pub const DEFAULT_TIMEOUT_MS: u64 = 4000;
pub const DEFAULT_TIMEOUT_PENALTY: f64 = 3000.0;
pub const DEFAULT_SLOW_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_ACTIVE_CAPACITY: usize = 5;
pub const DEFAULT_RETIRE_MAX_FAILURES: u32 = 5;
pub const DEFAULT_RETIRE_KEEP_ALIVE_SECS: u64 = 3600;
/// 淘汰地板：池子低于该数时不再摘（摘到 0 = 全黑，留着还有自愈机会）。
pub const DEFAULT_RETIRE_MIN_POOL: usize = 10;
pub const DEFAULT_LISTEN: &str = "127.0.0.1:17321";

/// TUN 透明代理默认值。
pub const DEFAULT_TUN_DEVICE: Option<&str> = None; // auto
pub const DEFAULT_TUN_MTU: Option<u16> = None; // auto
pub const DEFAULT_TUN_AUTO_ROUTE: bool = true;
pub const DEFAULT_TUN_FAKE_IP_CIDR: &str = "198.18.0.0/15";
pub const DEFAULT_TUN_EXCLUDE_CIDRS: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "127.0.0.0/8",
    "169.254.0.0/16",
];

/// gstatic generate_204：无 body、稳定，适合做延迟探测。
/// 仅 meow feature 下用到（真实探测），默认模式的 NoopMeasurer 不发请求。
#[cfg_attr(not(feature = "meow"), allow(dead_code))]
pub const DEFAULT_PROBE_URL: &str = "https://www.gstatic.com/generate_204";

/// 带宽探测端点：必须能下发有限字节 body（generate_204 无 body，不能用）。
/// cloudflare __down 按 `bytes=` 参数精确下发，是公开稳定的下载端点。
#[cfg_attr(not(feature = "meow"), allow(dead_code))]
pub const DEFAULT_BW_PROBE_URL: &str = "https://speed.cloudflare.com/__down?bytes=524288";

/// 带宽探测每 N 轮延迟测速跑一次。3 ≈ 4.5min（90s/轮）：
/// 每轮跑会让 selection 节点持续多背 512KB×N 的探测流量。
pub const DEFAULT_BW_INTERVAL_ROUNDS: u32 = 3;

/// 带宽探测单节点超时（ms）。比延迟探测长——要真下载数据，
/// 1Mbit 节点拉 512KB 需 4s，4s 超时会让慢节点永远测不到带宽（永远乐观）。
pub const DEFAULT_BW_TIMEOUT_MS: u64 = 8000;

/// 带宽探测最多读取的字节数，读满即断开。
pub const DEFAULT_BW_MAX_BYTES: u64 = 524288; // 512 KiB

/// 每降低 e 倍（≈2.72x）吞吐，排序上相当于加多少 ms 延迟（见 Node::score_with）。
pub const DEFAULT_BW_PENALTY_PER_EFOLD_MS: f64 = 1500.0;

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
