//! 协议域：节点规格（NodeSpec）解析与 meow adapter 构建。
//! silverq 不实现任何协议，只把节点规格喂给 meow。
pub mod dns;
#[cfg(feature = "meow")]
pub mod factory;
#[cfg(feature = "meow")]
pub mod meow;
pub mod nodespec;
