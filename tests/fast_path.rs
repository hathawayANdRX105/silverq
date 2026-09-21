use silverq::scheduler::batch::Measurement;
use silverq::scheduler::fast_path::*;
use silverq::scheduler::node::Node;
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
/// 「测了但一次没通」。死节点占多数的池子里后者是绝大多数，面板若不分开
/// 就会显示成 207 个"待测"，让人误判成调度漏测了节点（真实发生过）。
#[test]
fn failed_probe_still_marks_node_as_measured() {
    let mut nodes = vec![node("dead"), node("untouched")];
    assert!(nodes[0].last_measured.is_none());

    // 只给 "dead" 一个失败结果，"untouched" 不在这批里
    apply_batch(&mut nodes, &[timeout("dead")]);

    assert!(
        nodes[0].last_measured.is_some(),
        "失败的测速必须记录 last_measured，否则前端分不出「不可用」和「待测」"
    );
    assert_eq!(nodes[0].samples, 0, "失败不该增加成功样本数");
    assert!(!nodes[0].ewma.is_finite(), "从未成功过应保持 INFINITY");

    assert!(
        nodes[1].last_measured.is_none(),
        "没测的节点不该被标记为测过"
    );
}

/// 罚分绝不污染实测延迟，且一次成功即完全恢复。
///
/// 回归一个真 bug：早先失败时 `ewma += 3000` 并封顶 9999，导致
/// 1) 面板显示 7512ms 像是 2500ms 超时失效（实为 1512ms 真延迟 + 两次罚分）；
/// 2) 罚分是加法、恢复靠 alpha 混合（≤0.65），涨得比恢复快 ——
///    偶尔失败的活节点被永久压住，撞 9999 封顶后与真死节点无法区分
///    （线上实测 samples=77 的活节点显示 9999）。
#[test]
fn penalty_never_pollutes_measured_latency_and_recovers_instantly() {
    let mut pool = vec![node("flaky")];
    apply_batch(&mut pool, &[ok("flaky", 100.0)]);
    assert_eq!(pool[0].ewma, 100.0);

    // 连续失败很多次
    for _ in 0..50 {
        apply_batch(&mut pool, &[timeout("flaky")]);
    }
    assert_eq!(
        pool[0].ewma, 100.0,
        "实测延迟字段绝不能被罚分污染（面板就是读它）"
    );
    assert!(
        pool[0].score() > 100.0,
        "排序分数必须体现失败：score={}",
        pool[0].score()
    );

    // 一次成功立刻清零**罚分**（consecutive_failures），不需要多轮洗回来。
    // 稳定性窗口保留近期失败是刻意的：它看长期成功率，正是
    // consecutive_failures「一次成功就清零」掩盖的间歇性劣化盲区——
    // 所以这里的恢复是「罚分瞬时清零 + 稳定性逐步爬回」，不是分数回到 ewma。
    apply_batch(&mut pool, &[ok("flaky", 100.0)]);
    assert_eq!(pool[0].consecutive_failures, 0, "罚分必须一次成功即清零");
    assert!(
        pool[0].score() < 100.0 / 0.05,
        "稳定性地板兜底：全失败窗口下一次成功，分数不该仍顶在 20 倍：score={}",
        pool[0].score()
    );
    // 持续成功把窗口填满后，分数回到纯延迟
    for _ in 0..16 {
        apply_batch(&mut pool, &[ok("flaky", 100.0)]);
    }
    assert_eq!(
        pool[0].score(),
        pool[0].ewma,
        "窗口填满成功后排序分数必须等于实测延迟，score={} ewma={}",
        pool[0].score(),
        pool[0].ewma
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

/// max_failures == 0 = 禁用（面板热调开关）。
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
