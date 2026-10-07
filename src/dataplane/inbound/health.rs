//! 实际流量成败归因回调度节点池的健康度。

use crate::scheduler::node::Node;
use std::time::Duration;

use super::runtime::SharedPool;
/// 数据面近期失败冷却窗：实际流量里代理 dial 失败过的节点，
/// 在冷却窗内（`Node::last_runtime_failure`）被候选重排临时后置——
pub const RECENT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// 把实际流量成败归因回调度节点池的健康度（HP）。
///
/// 只写 HP 与 `last_runtime_failure`，不碰 `consecutive_failures`（那是
/// 调度侧「探测失败」计数，数据面失败不共享）：一次成功的调度探测
/// 会清掉失败计数、让节点复活，但冷却排序仍凭 `last_runtime_failure`
/// 把刚被实际流量证实失败的节点压在后面——两条证据线互不覆盖。
///
/// 成功 = 实际流量首字节成功（+HP，清瞬态标记）；失败 = 代理拨号失败
/// （-HP，打瞬态时刻）。目标无首字节只影响域名路线，客户端早退或
/// 竞速负方等中性结局不改变节点健康度。
pub async fn attribute_runtime(pool: &SharedPool, tag: &str, success: bool) {
    let mut nodes = pool.write().await;
    apply_runtime_outcome(&mut nodes, tag, success);
}

/// 实际流量结果的 HP 记账（纯函数，便于单测）。
///
/// 成功：`note_proxy_success`（+HP，清 `last_runtime_failure`）；
/// 失败：`note_proxy_failure`（-HP，打 `last_runtime_failure` 时刻）。
/// 池里没有该 tag（节点刚被 reload 摘除等）：静默跳过。
pub fn apply_runtime_outcome(nodes: &mut [Node], tag: &str, success: bool) {
    let Some(n) = nodes.iter_mut().find(|n| n.tag == tag) else {
        return;
    };
    if success {
        n.note_proxy_success();
    } else {
        n.note_proxy_failure();
    }
}

/// 候选重排：把冷却窗内失败过的节点后置，其余保持调度器 EWMA 顺序。
///
/// 失败证据来自 `Node::last_runtime_failure`（实际流量代理 dial 失败
/// 打的瞬态时刻，不是目标首字节缺失或调度测速的失败计数）。冷却窗外恢复原序——
pub fn reorder_recent_failures(
    mut order: Vec<String>,
    nodes: &[Node],
    cooldown: Duration,
) -> Vec<String> {
    let now = std::time::Instant::now();
    let mut current = 0;
    for _ in 0..order.len() {
        let failed_recently = nodes
            .iter()
            .find(|n| n.tag == order[current])
            .and_then(|n| n.last_runtime_failure)
            .is_some_and(|t| now.saturating_duration_since(t) < cooldown);
        if failed_recently {
            let tag = order.remove(current);
            order.push(tag);
        } else {
            current += 1;
        }
    }
    order
}
