//! TUN 数据面 —— **未实现**。占位模块，给后来的人（含 agent）留清楚的现场说明。
//!
//! # 为什么没做
//!
//! meow-rs **上游仓库里有 TUN**，但**没发布到 crates.io**。已发布的只有 5 个内核 crate：
//! `meow-common` / `meow-proxy` / `meow-transport` / `meow-dns` / `meow-trie`，
//! 里面没有任何 TUN 设备实现。
//!
//! 证据：`meow-common/src/outbound_iface.rs` 的模块文档明确提到 TUN 与其 issue 编号 ——
//!
//! > Outbound-socket interface binding for TUN global-route mode (#375).
//! > With `tun.auto-route: global` the split default routes send *all* IPv4
//! > traffic into the TUN device — including, without countermeasures, meow's
//! > own outbound sockets ... the TUN listener fail closed instead of starting
//! > a looping configuration.
//!
//! 也就是说上游不仅有 TUN，还有 `socket_protect` 那套"防止自己的 outbound 被 TUN
//! 路由回环"的机制。这些都在仓库里，crates.io 拿不到。
//!
//! # 要做的话怎么做
//!
//! 两条路，**先探再写，别盲写**：
//!
//! - **A（优先）**：把依赖从 crates.io 换成 `git` 依赖整个 meow-rs 仓库，
//!   直接复用上游 TUN + `socket_protect`。先确认 workspace 成员里 TUN crate 的名字和
//!   公开 API 形态。
//! - **B（兜底）**：silverq 自己用 `tun` + `smoltcp` 实现（shoes 走的就是这条路，
//!   见 `~/projects/shoes/src/tun/`：`tun_server.rs` / `tcp_stack_direct.rs` /
//!   `udp_manager.rs`）。工作量比 A 大一个量级，且要自己处理回环防护。
//!
//! # 与现有数据面的关系
//!
//! 当前 silverq 的数据面是 `inbound.rs`：SOCKS5（TCP + UDP ASSOCIATE）/ HTTP-CONNECT。
//! TUN 是**另一种** inbound（三层透明代理），不替换现有的，是并列新增。
//! 接进来之后同样调 `ProxyAdapter::dial_tcp` / `dial_udp`，
//! 复用现成的 EWMA 选择（`SharedSelection`），调度侧不用改。

/// 启动 TUN 数据面 —— 未实现。
///
/// 调用即 panic：宁可显式炸掉，也不要静默返回 Ok 让人以为 TUN 在跑。
/// 目前没有任何代码路径调用它（`main.rs` 不接这个模块的线）。
#[allow(dead_code)]
pub async fn run(device_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    unimplemented!(
        "TUN 数据面未实现（device={device_name}）。\
         meow-rs 上游有 TUN 但未发布到 crates.io，\
         需先改 git 依赖复用上游，或自行基于 tun+smoltcp 实现。\
         详见 src/tun.rs 模块文档。"
    )
}

/// 从 TUN 配置构建路由/回环防护 —— 未实现。
///
/// 上游对应 `socket_protect` / `outbound_iface`：TUN global-route 模式下必须
/// 把 meow 自己的 outbound socket 排除出 TUN 路由，否则流量绕回自己形成环路。
#[allow(dead_code)]
pub fn setup_route_protection() -> Result<(), Box<dyn std::error::Error>> {
    todo!(
        "TUN 回环防护未实现：auto-route 会把所有 IPv4 流量灌进 TUN，\
         必须先把自身 outbound socket 标记/绑定排除出去（参考 meow-common 的 \
         outbound_iface.rs 与 socket_protect.rs）。"
    )
}
