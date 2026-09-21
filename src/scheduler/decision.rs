//! Decision module: pure EWMA-based selection (no hysteresis rounds).
//! Nodes are ranked solely by current EWMA score.
//! Capacity constraint is enforced (top N become active).
use crate::scheduler::node::Node;

/// Select the top N nodes by EWMA score (lower is better).
/// Used to determine the active proxy group.
/// No hysteresis or round-based switching — pure score driven.
///
/// **从未测通的节点（ewma == INFINITY）一律排除**：它的分数加多少罚分都还是
/// INFINITY，凑进 selection 只是占着 active 名额让数据面去 dial 一个必死的节点，
/// 烧掉 fallback 槽位后还是兜底直连。池子半死时这会把死节点成批塞进队首
/// （实测 789 池只剩 3 个活节点时，剩 7 个名额被 nodes.yaml 开头的手工 VLESS
/// 死节点占走）。活节点不够 capacity 就少给，不拿死节点凑。
pub fn select_top(
    nodes: &[Node],
    capacity: usize,
    penalty_ms: f64,
    bw_penalty_per_efold_ms: f64,
) -> Vec<String> {
    let mut ranked: Vec<_> = nodes.iter().filter(|n| n.ewma.is_finite()).collect();
    ranked.sort_by(|a, b| {
        a.score_with(penalty_ms, bw_penalty_per_efold_ms)
            .partial_cmp(&b.score_with(penalty_ms, bw_penalty_per_efold_ms))
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
pub fn measurement_order(
    nodes: &[Node],
    batch_size: usize,
    penalty_ms: f64,
    bw_penalty_per_efold_ms: f64,
) -> Vec<Vec<Node>> {
    let (mut known, unmeasured): (Vec<Node>, Vec<Node>) =
        nodes.iter().cloned().partition(|n| n.samples > 0);
    known.sort_by(|a, b| {
        a.score_with(penalty_ms, bw_penalty_per_efold_ms)
            .partial_cmp(&b.score_with(penalty_ms, bw_penalty_per_efold_ms))
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
