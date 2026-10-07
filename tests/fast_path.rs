use silverq::scheduler::batch::Measurement;
use silverq::scheduler::fast_path::*;
use silverq::scheduler::node::{Node, HP_INITIAL};
use std::time::Instant;

fn node(tag: &str) -> Node {
    Node::new(tag, "127.0.0.1", 443)
}

fn ok(tag: &str, ms: f64) -> Measurement {
    Measurement {
        tag: tag.into(),
        delay_ms: Some(ms),
        bw_bps: None,
    }
}

fn timeout(tag: &str) -> Measurement {
    Measurement {
        tag: tag.into(),
        delay_ms: None,
        bw_bps: None,
    }
}

/// 从未成功的节点保持 INFINITY，天然排最后 —— 扣分对它无意义。
#[test]
fn never_measured_node_stays_worst() {
    let mut pool = vec![node("dead"), node("live")];
    apply_batch(&mut pool, &[timeout("dead"), ok("live", 50.0)]);

    assert!(pool[0].score().is_infinite(), "从未成功过应保持 INFINITY");
    assert_eq!(pool[1].score(), 50.0);
}

/// 关键场景：先活后死。扣分必须让它被健康节点超过，
/// 否则挂掉的节点会带着旧的好分数长期霸占队首。
#[test]
fn previously_healthy_node_is_penalized_after_failing() {
    let mut pool = vec![node("was_fast"), node("steady")];

    // 第一轮：was_fast 很快，steady 一般
    apply_batch(&mut pool, &[ok("was_fast", 20.0), ok("steady", 200.0)]);
    assert!(pool[0].score() < pool[1].score(), "was_fast 起初应更优");

    // was_fast 挂了：扣分后必须落到 steady 之后
    apply_batch(&mut pool, &[timeout("was_fast"), ok("steady", 200.0)]);
    assert!(
        pool[0].score() > pool[1].score(),
        "挂掉的节点必须被扣到 steady 之后：was_fast={} steady={}",
        pool[0].score(),
        pool[1].score()
    );
}

/// 失败的测速也必须记录"测过了"。
///
/// `samples` 只在成功时累加，所以 `samples == 0` 无法区分「还没轮到」和
/// 「测了但一次没通」。死节点占多数的池子里后者是绝大多数，ctl status 若不
/// 分开就会显示成 207 个"待测"，让人误判成调度漏测了节点（真实发生过）。
#[test]
fn failed_probe_still_marks_node_as_measured() {
    let mut nodes = vec![node("dead"), node("untouched")];
    assert!(nodes[0].last_measured.is_none());

    // 只给 "dead" 一个失败结果，"untouched" 不在这批里
    apply_batch(&mut nodes, &[timeout("dead")]);

    assert!(
        nodes[0].last_measured.is_some(),
        "失败的测速必须记录 last_measured，否则 ctl status 分不出「不可用」和「待测」"
    );
    assert_eq!(nodes[0].samples, 0, "失败不该增加成功样本数");
    assert!(!nodes[0].ewma.is_finite(), "从未成功过应保持 INFINITY");

    assert!(
        nodes[1].last_measured.is_none(),
        "没测的节点不该被标记为测过"
    );
}

/// 失败不能污染实测延迟；一次成功清连续失败，但稳定性和 HP 仍会保留近期风险。
///
/// 回归一个真 bug：早先失败时 `ewma += 3000` 并封顶 9999，导致
/// 1) ctl status 显示 7512ms 像是 2500ms 超时失效（实为 1512ms 真延迟 + 两次罚分）；
/// 2) 罚分是加法、恢复靠 alpha 混合（≤0.65），涨得比恢复快 ——
///    偶尔失败的活节点被永久压住，撞 9999 封顶后与真死节点无法区分
///    （线上实测 samples=77 的活节点显示 9999）。
#[test]
fn penalty_never_pollutes_measured_latency_and_recovers_with_successes() {
    let mut pool = vec![node("flaky"), node("steady")];
    apply_batch(&mut pool, &[ok("flaky", 100.0), ok("steady", 500.0)]);
    assert_eq!(pool[0].ewma, 100.0);

    // 连续失败很多次
    for _ in 0..50 {
        apply_batch(&mut pool, &[timeout("flaky")]);
    }
    assert_eq!(
        pool[0].ewma, 100.0,
        "实测延迟字段绝不能被罚分污染（ctl status 读的就是它）"
    );
    assert_eq!(
        silverq::scheduler::decision::select_top(&pool, 1, 3000.0, 1500.0),
        vec!["steady"],
        "连续失败后应由稳定备选承载新连接"
    );

    apply_batch(&mut pool, &[ok("flaky", 100.0)]);
    assert_eq!(pool[0].consecutive_failures, 0, "成功时连续失败应清零");
    assert_eq!(
        silverq::scheduler::decision::select_top(&pool, 1, 3000.0, 1500.0),
        vec!["steady"],
        "单次复活不能抹掉近期失败风险"
    );
    for _ in 0..50 {
        apply_batch(&mut pool, &[ok("flaky", 100.0)]);
    }
    assert_eq!(
        silverq::scheduler::decision::select_top(&pool, 1, 3000.0, 1500.0),
        vec!["flaky"],
        "持续成功后更快的节点应重新领先"
    );
}

/// 两级 pipeline：次数硬指标 + 时间延长保活。
/// samples==0 僵尸达到次数阈值即摘；曾通过的保活期内保留，期满才摘。
#[test]
fn retire_pipeline_two_levels() {
    let mut pool = vec![node("zombie"), node("tried"), node("healthy")];
    // zombie: 从未成功 + 连续失败 5 次
    let zs: Vec<Measurement> = (0..5).map(|_| timeout("zombie")).collect();
    apply_batch(&mut pool, &zs);
    // tried: 曾成功 2 次 + 连续失败 5 次（临时故障形态）
    let ts: Vec<Measurement> = [ok("tried", 100.0), ok("tried", 110.0)]
        .into_iter()
        .chain((0..5).map(|_| timeout("tried")))
        .collect();
    apply_batch(&mut pool, &ts);
    // healthy: 正常
    apply_batch(&mut pool, &[ok("healthy", 50.0)]);

    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 0);
    let tags: Vec<&str> = pool.iter().map(|n| n.tag.as_str()).collect();
    // zombie(5次失败,从未通过) 被摘; tried(5次失败,曾通过) 在保活期内保留
    assert_eq!(tags, vec!["tried", "healthy"], "僵尸摘、曾通过的保活");
    assert_eq!(retired, vec!["zombie"]);
}

/// 曾通过的节点连续失败持续满保活时长后才摘（时间指标）。
#[test]
fn retire_tried_node_after_keep_alive_expires() {
    let mut pool = vec![node("old-tried")];
    let oks: Vec<Measurement> = (0..3).map(|_| ok("old-tried", 90.0)).collect();
    apply_batch(&mut pool, &oks);
    let tos: Vec<Measurement> = (0..6).map(|_| timeout("old-tried")).collect();
    apply_batch(&mut pool, &tos);
    // 手动把 failing_since 拨回 2 小时前
    pool[0].failing_since = Some(Instant::now() - std::time::Duration::from_secs(7200));

    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 0);
    assert_eq!(retired, vec!["old-tried"], "保活期满，摘除");
    assert!(pool.is_empty());
}

/// 保活期内（< keep_alive）曾通过的节点不摘。
#[test]
fn retire_tried_node_kept_within_keep_alive() {
    let mut pool = vec![node("new-tried")];
    let oks: Vec<Measurement> = (0..3).map(|_| ok("new-tried", 90.0)).collect();
    apply_batch(&mut pool, &oks);
    let tos: Vec<Measurement> = (0..6).map(|_| timeout("new-tried")).collect();
    apply_batch(&mut pool, &tos);
    pool[0].failing_since = Some(Instant::now() - std::time::Duration::from_secs(600)); // 10 分钟

    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 0);
    assert!(retired.is_empty(), "保活期内保留");
    assert_eq!(pool.len(), 1);
}

/// max_failures == 0 = 禁用（ctl config-reload 热调开关）。
#[test]
fn retire_disabled_at_zero() {
    let mut pool = vec![node("zombie")];
    let zs: Vec<Measurement> = (0..9).map(|_| timeout("zombie")).collect();
    apply_batch(&mut pool, &zs);
    let retired = retire_stale(&mut pool, 0, std::time::Duration::from_secs(3600), 0);
    assert!(retired.is_empty());
    assert_eq!(pool.len(), 1);
}

/// 连续失败未达阈值的不摘 —— 阈值就是保命宽限。
#[test]
fn retire_respects_threshold_grace() {
    let mut pool = vec![node("fresh-dead")];
    let ds: Vec<Measurement> = (0..2).map(|_| timeout("fresh-dead")).collect();
    apply_batch(&mut pool, &ds);
    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 0);
    assert!(retired.is_empty());
    assert_eq!(pool.len(), 1);
}

/// 淘汰地板：摘除后池子不得低于 min_pool，回填最可能活的
/// （连续失败次数少者优先，其次 samples 多者优先）。
#[test]
fn retire_floor_backfills_best_candidates() {
    let mut pool = vec![node("zombie-a"), node("zombie-b"), node("was-alive")];
    let za: Vec<Measurement> = (0..9).map(|_| timeout("zombie-a")).collect();
    apply_batch(&mut pool, &za);
    let zb: Vec<Measurement> = (0..9).map(|_| timeout("zombie-b")).collect();
    apply_batch(&mut pool, &zb);
    // 曾通过 42 次 + 连续失败 9 次；keep_alive=0 让它进 retired 候选
    let wa: Vec<Measurement> = (0..42)
        .map(|_| ok("was-alive", 100.0))
        .chain((0..9).map(|_| timeout("was-alive")))
        .collect();
    apply_batch(&mut pool, &wa);

    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(0), 2);
    let tags: Vec<&str> = pool.iter().map(|n| n.tag.as_str()).collect();
    assert_eq!(pool.len(), 2, "回填到地板: {tags:?}");
    assert!(
        tags.contains(&"was-alive"),
        "samples 最多的被回填: {tags:?}"
    );
    assert_eq!(retired, vec!["zombie-b".to_string()]);
}

/// 地板 ≥ 池大小时一个不摘（全黑防线）。
#[test]
fn retire_floor_keeps_everything_when_large() {
    let mut pool = vec![node("zombie")];
    let zs: Vec<Measurement> = (0..5).map(|_| timeout("zombie")).collect();
    apply_batch(&mut pool, &zs);
    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(0), 10);
    assert_eq!(pool.len(), 1);
    assert!(retired.is_empty());
}

// ── 根因回归：带宽成功曾被误计为失败 ──────────────────────────────────────

/// 带宽成功（`delay_ms=None, bw_bps=Some`）必须清失败计数与计时，
/// 否则 `retire_stale` 凭旧 `failing_since` 把刚证明自己还活着的节点摘掉
/// （线上回归：first_choice 从 fast 掉到 backup）。
///
/// 场景按真实时序复刻：延迟测通 → 5 次延迟失败（超 1h 保活线还在计时）
/// → 带宽探测成功 → `retire_stale` 必须保留该节点。
#[test]
fn bw_success_clears_failure_streak_and_saves_node_from_retirement() {
    let mut pool = vec![node("fast"), node("backup")];

    // 先测通（samples>0，进入保活语义），随后 5 次延迟失败
    apply_batch(&mut pool, &[ok("fast", 100.0)]);
    for _ in 0..5 {
        apply_batch(&mut pool, &[timeout("fast")]);
    }
    assert_eq!(pool[0].consecutive_failures, 5);
    assert!(pool[0].failing_since.is_some());

    // 模拟保活计时已超 1h 线（把 failing_since 拨到 2h 前）
    pool[0].failing_since = Some(Instant::now() - std::time::Duration::from_secs(2 * 3600));

    // 带宽成功到达：存活证据 → 清失败计数、复位计时、+1 健康度
    let bw = Measurement {
        tag: "fast".into(),
        delay_ms: None,
        bw_bps: Some(500_000.0),
    };
    apply_batch(&mut pool, &[bw]);
    assert_eq!(pool[0].consecutive_failures, 0, "带宽成功必须清失败计数");
    assert!(pool[0].failing_since.is_none(), "带宽成功必须复位保活计时");
    assert!(pool[0].bw_bps().is_some(), "带宽样本必须落库");

    // 1h 保活期 + 5 次阈值：现在两条线都不再满足 → 不摘
    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 1);
    assert!(
        retired.is_empty(),
        "带宽成功过的活节点不能被 retire_stale 摘掉：{retired:?}"
    );

    // 健康度记账：50 +1(延迟成功) -25(5次×5) +1(带宽成功) = 27。
    // 守「每次探测恰好 +1 一次」：同一次测量里延迟+带宽共存不重复加分。
    assert_eq!(pool[0].hp, HP_INITIAL + 1 - 25 + 1);
}

/// 混合成功（延迟+带宽同在一次测量里）也必须只记一次探测成功 +1，
/// 且两条 EWMA 都更新。
#[test]
fn mixed_delay_and_bw_success_counts_probe_once() {
    let mut pool = vec![node("a")];
    apply_batch(&mut pool, &[ok("a", 100.0)]);
    let mixed = Measurement {
        tag: "a".into(),
        delay_ms: Some(90.0),
        bw_bps: Some(800_000.0),
    };
    apply_batch(&mut pool, &[mixed]);
    assert_eq!(pool[0].hp, HP_INITIAL + 2, "两次探测各 +1，混合测量不重复");
    assert!(pool[0].bw_bps().is_some());
}

/// 带宽侧超时（两者皆 None）仍是失败：计数与扣健康度照旧——
/// 只有「有值」才算存活证据。
#[test]
fn bw_timeout_still_penalizes() {
    let mut pool = vec![node("a")];
    apply_batch(&mut pool, &[ok("a", 100.0)]);
    apply_batch(&mut pool, &[timeout("a")]);
    assert_eq!(pool[0].consecutive_failures, 1);
    assert_eq!(pool[0].hp, HP_INITIAL + 1 - 5);
}

// ── 回归（评审 P2）：「被流量命中但从未被探测」的节点 ───────────────────
//
// 这类节点 `samples==0`（探测还没轮到它），但实际流量首字节成功
// （`note_proxy_success`）是真实存活证据。重启丢失证据或探测失败攒满
// 阈值后，旧逻辑按「无证据僵尸」立即摘除——尽管它真实在响应。

/// 有实际流量证据的节点（`samples==0` 但 `ever_responded`）连续 5 次
/// 探测失败：在保活宽限内与「曾测通过」同权保留，不按无证据僵尸
/// 立即摘除（回归评审 P2：重启丢证据后 5 次探测失败即被摘）。
#[test]
fn traffic_live_unprobed_node_kept_for_grace() {
    let mut pool = vec![node("ghost")];
    // 从未被探测（samples 保持 0），但拿过实际流量首字节
    pool[0].note_proxy_success();
    assert_eq!(pool[0].samples, 0);
    assert!(pool[0].ever_responded);
    let tos: Vec<Measurement> = (0..5).map(|_| timeout("ghost")).collect();
    apply_batch(&mut pool, &tos);
    assert_eq!(pool[0].consecutive_failures, 5);
    // 把保活计时拨到 10 分钟前（仍在 1h 宽限内）
    pool[0].failing_since = Some(Instant::now() - std::time::Duration::from_secs(600));

    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 0);
    assert!(
        retired.is_empty(),
        "有实际流量证据的节点宽限期内不能摘除：{retired:?}"
    );
    assert_eq!(pool.len(), 1);
}

/// 证据只买宽限，不买永生：实际流量证据节点连续失败持续满 `keep_alive`
/// 后仍被淘汰。
#[test]
fn traffic_live_unprobed_node_retires_after_keep_alive_expires() {
    let mut pool = vec![node("ghost")];
    pool[0].note_proxy_success();
    let tos: Vec<Measurement> = (0..5).map(|_| timeout("ghost")).collect();
    apply_batch(&mut pool, &tos);
    // 保活计时拨到 3h 前（> 1h 保活线）
    pool[0].failing_since = Some(Instant::now() - std::time::Duration::from_secs(3 * 3600));

    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 0);
    assert_eq!(retired, vec!["ghost".to_string()], "保活期满证据节点也要摘");
    assert!(pool.is_empty());
}

/// 从未成功、也从未拿过实际流量响应的节点：连续失败达阈值立即摘除
/// （无先验证据，不存在"临时故障"，无保活价值）。
#[test]
fn never_responded_node_retired_immediately() {
    let mut pool = vec![node("never")];
    let tos: Vec<Measurement> = (0..5).map(|_| timeout("never")).collect();
    apply_batch(&mut pool, &tos);
    assert!(!pool[0].ever_responded);

    let retired = retire_stale(&mut pool, 5, std::time::Duration::from_secs(3600), 0);
    assert_eq!(retired, vec!["never".to_string()], "无证据僵尸立即摘");
    assert!(pool.is_empty());
}
