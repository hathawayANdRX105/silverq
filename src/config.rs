//! Batch configuration and defaults.
//! All values chosen to match user requirements:
//! - Ranked order batching (best nodes first)
//! - Timeout nodes heavily penalized and moved to back
pub const DEFAULT_BATCH_SIZE: usize = 5;
pub const DEFAULT_CONCURRENCY: usize = 6;
pub const DEFAULT_TIMEOUT_MS: u64 = 2000;
pub const DEFAULT_TIMEOUT_PENALTY: f64 = 3000.0;
pub const DEFAULT_SLOW_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_ACTIVE_CAPACITY: usize = 5;