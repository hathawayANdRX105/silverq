//! 控制面：unix socket ctl 协议 + 子命令解析。仅 unix。
pub mod cli;
pub mod protocol;

// 常用项上提一层，避免 crate::ctl::protocol:: 双层写法。
#[cfg(feature = "meow")]
pub use protocol::do_select_public;
pub use protocol::SharedTuning;
