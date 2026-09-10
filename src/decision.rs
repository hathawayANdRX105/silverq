//! Decision module: pure EWMA-based selection (no hysteresis rounds).
//! Nodes are ranked solely by current EWMA score.
//! Capacity constraint is enforced (top N become active).
use crate::node::Node;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::DEFAULT_FAILURE_PENALTY_MS;

    fn measured(tag: &str, ms: f64) -> Node {
        let mut n = Node::new(tag, "1.1.1.1", 443);
        n.update(ms);
        n
    }

    #[test]
    fn select_top_orders_by_score_and_respects_capacity() {
        let pool = vec![
            measured("slow", 500.0),
            measured("fast", 50.0),
            Node::new("never", "2.2.2.2", 443),
            measured("mid", 200.0),
        ];
        assert_eq!(
            select_top(&pool, 2, DEFAULT_FAILURE_PENALTY_MS),
            vec!["fast", "mid"]
        );
        assert_eq!(
            select_top(&pool, 10, DEFAULT_FAILURE_PENALTY_MS).len(),
            4,
            "capacity 超池大小时取全部"
        );
        // 没测过的排最后
        assert_eq!(select_top(&pool, 4, DEFAULT_FAILURE_PENALTY_MS)[3], "never");
    }

    /// 交错的核心保证：未测节点不会被挤到最后一批，第一批就必须包含它们。
    #[test]
    fn first_batch_mixes_known_and_unmeasured() {
        // 场景必须让已知节点数远多于 batch_size：否则未测节点会因凑不满一批
        // 而"顺带"进入第一批，断言恒真，测不出不交错（变异检验发现过该漏洞）。
        let mut pool: Vec<Node> = (0..20)
            .map(|i| measured(&format!("k{i:02}"), (i + 1) as f64 * 10.0))
            .collect();
        for i in 0..20 {
            pool.push(Node::new(format!("u{i}"), "2.2.2.2", 443));
        }

        let batches = measurement_order(&pool, 4, DEFAULT_FAILURE_PENALTY_MS);
        let first: Vec<&str> = batches[0].iter().map(|n| n.tag.as_str()).collect();

        assert!(
            first.iter().any(|t| t.starts_with('k')),
            "第一批应含已知节点: {first:?}"
        );
        assert!(
            first.iter().any(|t| t.starts_with('u')),
            "第一批必须含未测节点，否则活节点要等很久才发现: {first:?}"
        );
        // 已知节点按分数升序进入
        assert_eq!(first[0], "k00");
    }

    #[test]
    fn every_node_is_measured_exactly_once() {
        let mut pool = vec![measured("k1", 10.0)];
        for i in 0..9 {
            pool.push(Node::new(format!("u{i}"), "2.2.2.2", 443));
        }

        let batches = measurement_order(&pool, 3, DEFAULT_FAILURE_PENALTY_MS);
        let mut seen: Vec<String> = batches
            .iter()
            .flat_map(|b| b.iter().map(|n| n.tag.clone()))
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 10, "每个节点必须且只被测一次");
    }

    #[test]
    fn handles_all_unmeasured_and_all_known() {
        let all_new: Vec<Node> = (0..7)
            .map(|i| Node::new(format!("u{i}"), "1.1.1.1", 443))
            .collect();
        let b = measurement_order(&all_new, 3, DEFAULT_FAILURE_PENALTY_MS);
        assert_eq!(b.iter().map(|x| x.len()).sum::<usize>(), 7);

        let all_known: Vec<Node> = (0..5)
            .map(|i| measured(&format!("k{i}"), i as f64 * 10.0 + 1.0))
            .collect();
        let b = measurement_order(&all_known, 2, DEFAULT_FAILURE_PENALTY_MS);
        assert_eq!(b.iter().map(|x| x.len()).sum::<usize>(), 5);
    }
}
