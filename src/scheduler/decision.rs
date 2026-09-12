//! Decision module: pure EWMA-based selection (no hysteresis rounds).
//! Nodes are ranked solely by current EWMA score.
//! Capacity constraint is enforced (top N become active).
use crate::scheduler::node::Node;

/// Select the top N nodes by EWMA score (lower is better).
/// Used to determine the active proxy group.
/// No hysteresis or round-based switching — pure score driven.
pub fn select_top(nodes: &[Node], capacity: usize, penalty_ms: f64) -> Vec<String> {
    let mut ranked: Vec<_> = nodes.iter().collect();
    ranked.sort_by(|a, b| {
        a.score_with(penalty_ms)
            .partial_cmp(&b.score_with(penalty_ms))
            .unwrap()
    });
    ranked
        .into_iter()
        .take(capacity)
        .map(|n| n.tag.clone())
        .collect()
}

/// 把节点池排成测速顺序：已测过的按分数升序，未测过的按原序，
/// 然后每批混合两者（每批前 `half` 个取已知、其余补未知）。
///
/// 为什么要交错：初始时所有节点分数都是 `INFINITY`，纯按分数排序等于配置顺序，
/// 活节点若排在池子后部会很久测不到。实测真实池 217 节点时，50s 内测了 99 个
/// 全失败，而同时 sing-box 已测出 7 个活节点 —— 它们都排在后面还没轮到。
pub fn measurement_order(nodes: &[Node], batch_size: usize, penalty_ms: f64) -> Vec<Vec<Node>> {
    let (mut known, unmeasured): (Vec<Node>, Vec<Node>) =
        nodes.iter().cloned().partition(|n| n.samples > 0);
    known.sort_by(|a, b| {
        a.score_with(penalty_ms)
            .partial_cmp(&b.score_with(penalty_ms))
            .unwrap()
    });

    let half = (batch_size / 2).max(1);
    let mut known = known.into_iter();
    let mut unmeasured = unmeasured.into_iter();
    let mut batches = Vec::new();

    loop {
        let mut chunk: Vec<Node> = Vec::with_capacity(batch_size);
        for _ in 0..half {
            if let Some(n) = known.next() {
                chunk.push(n);
            }
        }
        while chunk.len() < batch_size {
            match unmeasured.next() {
                Some(n) => chunk.push(n),
                None => match known.next() {
                    Some(n) => chunk.push(n),
                    None => break,
                },
            }
        }
        if chunk.is_empty() {
            return batches;
        }
        batches.push(chunk);
    }
}
