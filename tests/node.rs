//! 吞吐感知调度的核心不变量：对数域 EWMA、稳定性乘子、三者融合的排序。
//!
//! 这些测试守的是「排序为什么是这样」的语义，不是字段拷贝：
//! - 重尾分布下线性 EWMA 会给出不存在的中间值（对数域才对）
//! - 半死节点必须被乘性压下去（加性罚分压不住）
//! - 未测带宽的节点不奖不罚（乐观初值，探索）
use silverq::scheduler::node::{Node, DEFAULT_BW_PENALTY_PER_EFOLD_MS, DEFAULT_FAILURE_PENALTY_MS};

/// 稳定性窗口里塞入指定数量的成败结果。
fn outcomes(n: &mut Node, oks: &[bool]) {
    for &ok in oks {
        n.record_outcome(ok);
    }
}

#[test]
fn log_domain_bw_tracks_heavy_tail_not_linear_mean() {
    // 重尾样本：1MB/s 与 10KB/s 交替。线性均值 = 505KB/s（两个都不是）；
    // 对数域几何均值 ≈ 100KB/s，贴合「这节点时快时慢，体感是慢的」。
    let mut n = Node::new("mix", "1.1.1.1", 443);
    for (i, bps) in [1_048_576.0, 10_240.0].iter().cycle().enumerate().take(8) {
        n.update_bw(*bps);
        let _ = i;
    }
    let got = n.bw_bps().expect("测过 8 次必须有带宽值");
    assert!(
        got < 200_000.0,
        "对数域 EWMA 应贴近几何均值 ~100KB/s，got {got:.0} B/s（线性均值会是 505KB/s）"
    );
    assert!(got > 20_000.0, "不该被单个最慢样本拽到地板，got {got:.0}");
}

#[test]
fn bw_ewma_moves_toward_new_samples() {
    // 长期慢 → 突然持续快：分数必须能爬上来，否则调度被旧观测永久锁死。
    let mut n = Node::new("recover", "1.1.1.1", 443);
    for _ in 0..10 {
        n.update_bw(20_000.0);
    }
    let slow = n.bw_bps().unwrap();
    for _ in 0..15 {
        n.update_bw(2_000_000.0);
    }
    let fast = n.bw_bps().unwrap();
    assert!(
        fast > slow * 20.0,
        "持续快样本后带宽必须显著上升: {slow:.0} → {fast:.0}"
    );
}

#[test]
fn zero_and_negative_bw_are_ignored() {
    let mut n = Node::new("zero", "1.1.1.1", 443);
    n.update_bw(0.0);
    n.update_bw(-1.0);
    assert!(n.bw_bps().is_none(), "bps<=0 对数域无定义，必须静默丢弃");
    // 丢弃后状态干净，后续合法采样照常工作
    n.update_bw(100_000.0);
    assert!(n.bw_bps().is_some());
}

#[test]
fn unmeasured_bw_neither_rewarded_nor_penalized() {
    // 乐观初值：没测过带宽的节点分数只由延迟+稳定性决定。
    // 奖励它会让它永远占据队首（测一次就赢）；惩罚它则冷启动时全是 INFINITY 惩罚。
    let mut measured = Node::new("measured", "1.1.1.1", 443);
    measured.update(100.0);
    measured.update_bw(1_000.0); // 极慢 1KB/s
    let measured_score =
        measured.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS);

    let mut fresh = Node::new("fresh", "1.1.1.1", 443);
    fresh.update(100.0);
    let fresh_score = fresh.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS);

    assert!(
        fresh_score < measured_score,
        "未测带宽应中性（=纯延迟分），极慢节点应被罚分压下: fresh={fresh_score:.0} measured={measured_score:.0}"
    );
}

#[test]
fn stability_scales_score_by_inverse_success_rate() {
    // 乘性语义：50% 成功率把有效代价翻倍（300ms → 600ms）。
    // 这不是「翻倍到 1200ms」——期望代价分析下，50% 的请求要重试一次
    // 下一跳，E[cost] = 0.5*300 + 0.5*(300+下一跳代价)，1/stability 是
    // 它的合理下界近似，不是精确重算。
    let mut flaky = Node::new("flaky", "1.1.1.1", 443);
    flaky.update(300.0);
    outcomes(&mut flaky, &[true, false, true, false, true, false]);
    assert_eq!(flaky.stability(), 0.5);
    let flaky_score = flaky.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS);
    assert!(
        (flaky_score - 600.0).abs() < 1e-6,
        "300ms/50% → 600，got {flaky_score}"
    );

    // 乘性能在延迟差不够大时翻转排序（加性固定罚分做不到成比例压制）
    let mut steady = Node::new("steady", "1.1.1.1", 443);
    steady.update(500.0);
    outcomes(&mut steady, &[true; 6]);
    let steady_score =
        steady.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS);
    assert!((steady_score - 500.0).abs() < 1e-6);
    assert!(
        steady_score < flaky_score,
        "500ms 稳定节点必须排在 300ms/50% 节点前面: steady={steady_score:.0} flaky={flaky_score:.0}"
    );
}

#[test]
fn empty_outcome_window_means_neutral_stability() {
    // 无数据不惩罚：新节点和重启后的节点不该被假设成不可靠。
    let mut n = Node::new("new", "1.1.1.1", 443);
    n.update(100.0);
    assert_eq!(n.stability(), 1.0);
    let neutral = n.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS);
    // 塞满成功后分数应完全不变（稳定性仍 1.0）
    outcomes(&mut n, &[true; 16]);
    let healthy = n.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS);
    assert_eq!(neutral, healthy);
}

#[test]
fn all_fail_outcome_window_keeps_score_finite() {
    // 稳定性为 0 时除零必须有地板，否则分数变 inf 破坏可排序性。
    let mut n = Node::new("dead", "1.1.1.1", 443);
    n.update(80.0);
    outcomes(&mut n, &[false; 16]);
    assert_eq!(n.stability(), 0.0);
    let score = n.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS);
    assert!(
        score.is_finite(),
        "全失败时节点的分数仍必须有限可排序，got {score}"
    );
}

#[test]
fn fusion_orders_fast_stable_node_above_slow_one() {
    // 融合排序的端到端断言：延迟差 3 倍、稳定性相同、带宽差 16 倍时，
    // 快节点必须排第一。这是「吞吐感知」存在的全部理由。
    let mut slow = Node::new("slow", "1.1.1.1", 443);
    slow.update(300.0);
    slow.update_bw(16_384.0); // 16KB/s
    outcomes(&mut slow, &[true; 8]);

    let mut fast = Node::new("fast", "1.1.1.1", 443);
    fast.update(900.0);
    fast.update_bw(262_144.0); // 256KB/s
    outcomes(&mut fast, &[true; 8]);

    let (fs, ss) = (
        fast.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS),
        slow.score_with(DEFAULT_FAILURE_PENALTY_MS, DEFAULT_BW_PENALTY_PER_EFOLD_MS),
    );
    assert!(
        fs < ss,
        "256KB/s 快节点必须排在 16KB/s 慢节点前面（即使延迟贵 3 倍）: fast={fs:.0} slow={ss:.0}"
    );
}

#[test]
fn bw_penalty_scale_is_monotone_in_speed() {
    // 惩罚权重随吞吐单调：越慢罚越重，且对数尺度上等比。
    // 守的是「每 e 倍吞吐差 = 固定 ms」的线性可加性。
    let penalty = DEFAULT_BW_PENALTY_PER_EFOLD_MS;
    let mut a = Node::new("a", "1.1.1.1", 443);
    a.update(100.0);
    a.update_bw(100_000.0);
    let mut b = Node::new("b", "1.1.1.1", 443);
    b.update(100.0);
    b.update_bw(100_000.0 * std::f64::consts::E); // 快 e 倍
    let mut c = Node::new("c", "1.1.1.1", 443);
    c.update(100.0);
    c.update_bw(100_000.0 / std::f64::consts::E); // 慢 e 倍

    let sa = a.score_with(DEFAULT_FAILURE_PENALTY_MS, penalty);
    let sb = b.score_with(DEFAULT_FAILURE_PENALTY_MS, penalty);
    let sc = c.score_with(DEFAULT_FAILURE_PENALTY_MS, penalty);
    assert!(
        sb < sa && sa < sc,
        "必须快<基准<慢: {sb:.0} < {sa:.0} < {sc:.0}"
    );
    // 每差 e 倍吞吐，分数差应 ≈ 一个 penalty 单位（对数尺度线性）
    assert!(
        (sa - sb - penalty).abs() < 1.0,
        "快 e 倍应减一个 penalty: {}",
        sa - sb
    );
    assert!(
        (sc - sa - penalty).abs() < 1.0,
        "慢 e 倍应加一个 penalty: {}",
        sc - sa
    );
}

#[test]
fn adopt_score_carries_bw_state_for_reload() {
    // reload 时新 Node 接管旧分数：带宽状态必须一起过来，否则每次
    // freenode-pool reload（约 13min 一次）都丢掉吞吐观测。
    let mut old = Node::new("x", "1.1.1.1", 443);
    old.update(150.0);
    old.update_bw(500_000.0);
    outcomes(&mut old, &[true, true, false]);

    let mut new = Node::new("x", "1.1.1.1", 443);
    new.adopt_score(&old);
    assert_eq!(new.ewma, 150.0);
    assert!(new.bw_bps().is_some(), "adopt 必须带走带宽分数");
    assert_eq!(new.stability(), old.stability(), "adopt 必须带走稳定性窗口");
}

#[test]
fn restore_bw_rejects_invalid_values() {
    // 存档可能被手改：非法值必须被拒，保持乐观初值而非污染排序。
    let mut n = Node::new("r", "1.1.1.1", 443);
    n.restore_bw(f64::NAN, 5);
    assert!(n.bw_bps().is_none(), "NaN bw_log 必须被拒");
    n.restore_bw(12.0, 0);
    assert!(n.bw_bps().is_none(), "bw_samples=0 必须被拒");
    n.restore_bw(12.0, 3);
    assert!(n.bw_bps().is_some(), "合法值必须接受");
}
