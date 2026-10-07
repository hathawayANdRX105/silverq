//! 数据面：最小 SOCKS5/HTTP-CONNECT 混合 inbound。
//! 转发目标 = 调度循环维护的"当前 top-N 选择"，经 meow adapter 直连。
//! silverq 自己就是 selector；meow 只负责协议与连接。
#![cfg(feature = "meow")]

mod direct;
mod health;
mod policy;
mod protocol;
mod race;
mod relay;
mod runtime;
mod serve;
mod udp;

pub use health::{
    apply_runtime_outcome, attribute_runtime, reorder_recent_failures, RECENT_FAILURE_COOLDOWN,
};
pub use policy::{is_local_target, pick_candidates, route_cache_eligible};
pub use protocol::{read_http_connect_target, read_socks5_target, socks5_greeting};
pub use race::{race_relay, replay_race_safe, RaceCtx, RaceOutcome, SideFut, SideSpec};
pub use relay::{relay, RelayFail, RelayOutcome, RouteRecorder};
pub use runtime::{
    run, DialTuning, InboundRuntime, SharedPool, SharedSelection, SharedTuning, Target,
};
