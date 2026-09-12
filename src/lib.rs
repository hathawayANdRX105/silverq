//! silverq — 稳定性优先的代理节点调度器。
//!
//! 模块树（按域划分）：
//! - [`config`]   默认参数/环境变量覆盖 + silverq.toml
//! - [`scheduler`] 节点池 + EWMA + 分批测速 + 选择 + 存档
//! - [`proxy`]     NodeSpec 解析 + meow adapter 构建（meow feature）
//! - [`dataplane`] SOCKS5/HTTP-CONNECT inbound + UDP 中继（meow feature）
//! - [`web`]       内嵌面板（meow feature）
//! - [`ctl`]       unix socket 控制通道 + 子命令解析（unix）
//!
//! 协议/传输/TLS/Reality/QUIC 全部复用 meow-rs；silverq 只做调度与转发。
pub mod config;
#[cfg(unix)]
pub mod ctl;
#[cfg(feature = "meow")]
pub mod dataplane;
pub mod proxy;
pub mod scheduler;
#[cfg(feature = "meow")]
pub mod web;
