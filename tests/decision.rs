use silverq::scheduler::decision::*;
use silverq::scheduler::node::Node;
use silverq::scheduler::node::{DEFAULT_BW_PENALTY_PER_EFOLD_MS, DEFAULT_FAILURE_PENALTY_MS};

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
        select_top(
            &pool,
            2,
            DEFAULT_FAILURE_PENALTY_MS,
            DEFAULT_BW_PENALTY_PER_EFOLD_MS
        ),
        vec!["fast", "mid"]
    );
    // 从未测通的节点不进 selection：进去只是占 active 名额让数据面 dial 必死节点
    assert_eq!(
        select_top(
            &pool,
            10,
            DEFAULT_FAILURE_PENALTY_MS,
            DEFAULT_BW_PENALTY_PER_EFOLD_MS
        ),
        vec!["fast", "mid", "slow"],
        "capacity 超活节点数时只给活的，不拿没测过的凑数"
    );
    assert!(
        !select_top(
            &pool,
            4,
            DEFAULT_FAILURE_PENALTY_MS,
            DEFAULT_BW_PENALTY_PER_EFOLD_MS
        )
        .contains(&"never".to_string()),
        "从未测通的节点绝不能出现在 selection"
    );
}

/// 整池都没有活节点时返回空，调度循环据此保留冷启动种子（见 main.rs）
#[test]
fn select_top_all_unmeasured_returns_empty() {
    let pool: Vec<Node> = (0..5)
        .map(|i| Node::new(format!("u{i}"), "2.2.2.2", 443))
        .collect();
    assert!(select_top(
        &pool,
        10,
        DEFAULT_FAILURE_PENALTY_MS,
        DEFAULT_BW_PENALTY_PER_EFOLD_MS
    )
    .is_empty());
}

/// 曾测通但当前连续失败的节点仍保留资格（罚分在 score 里算，靠后但不除名），
/// 一次成功即复活。
#[test]
fn select_top_keeps_failing_but_once_alive_nodes() {
    let mut dead_now = measured("was-alive", 300.0);
    dead_now.penalize();
    dead_now.penalize();
    let pool = vec![dead_now, Node::new("fresh", "2.2.2.2", 443)];
    let sel = select_top(
        &pool,
        5,
        DEFAULT_FAILURE_PENALTY_MS,
        DEFAULT_BW_PENALTY_PER_EFOLD_MS,
    );
    assert_eq!(sel, vec!["was-alive"]);
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

    let batches = measurement_order(
        &pool,
        4,
        DEFAULT_FAILURE_PENALTY_MS,
        DEFAULT_BW_PENALTY_PER_EFOLD_MS,
    );
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

    let batches = measurement_order(
        &pool,
        3,
        DEFAULT_FAILURE_PENALTY_MS,
        DEFAULT_BW_PENALTY_PER_EFOLD_MS,
    );
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
    let b = measurement_order(
        &all_new,
        3,
        DEFAULT_FAILURE_PENALTY_MS,
        DEFAULT_BW_PENALTY_PER_EFOLD_MS,
    );
    assert_eq!(b.iter().map(|x| x.len()).sum::<usize>(), 7);

    let all_known: Vec<Node> = (0..5)
        .map(|i| measured(&format!("k{i}"), i as f64 * 10.0 + 1.0))
        .collect();
    let b = measurement_order(
        &all_known,
        2,
        DEFAULT_FAILURE_PENALTY_MS,
        DEFAULT_BW_PENALTY_PER_EFOLD_MS,
    );
    assert_eq!(b.iter().map(|x| x.len()).sum::<usize>(), 5);
}

/// 回归：`adopt_score` 必须接管 `ewma`。
///
/// reload 路径（ctl `do_reload`）按 tag 建全新 Node 池再逐节点
/// `adopt_score(o)` 继承分数，随后直接 `select_top` 写 selection。
/// `select_top` 按 `ewma.is_finite()` 过滤 —— 一旦 adopt 丢掉 ewma，
/// reload 后整池 INFINITY 被滤空，selection 清空，数据面没有候选可拨
/// （代理静默退化成纯直连，直到下一轮测速重新攒出分数）。
#[test]
fn reload_adopts_scores_so_selection_survives() {
    let old = [measured("a", 40.0), measured("b", 80.0)];
    // 模拟 reload：同 tag 全新节点
    let mut new_pool: Vec<Node> = old
        .iter()
        .map(|o| Node::new(o.tag.clone(), o.server.clone(), o.port))
        .collect();
    for n in new_pool.iter_mut() {
        if let Some(o) = old.iter().find(|o| o.tag == n.tag) {
            n.adopt_score(o);
        }
    }
    let top = select_top(
        &new_pool,
        10,
        DEFAULT_FAILURE_PENALTY_MS,
        DEFAULT_BW_PENALTY_PER_EFOLD_MS,
    );
    assert_eq!(
        top,
        vec!["a".to_string(), "b".to_string()],
        "reload 后 selection 不能空"
    );
}
