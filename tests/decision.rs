use silverq::scheduler::decision::*;
use silverq::scheduler::node::Node;
use silverq::scheduler::node::DEFAULT_FAILURE_PENALTY_MS;

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
