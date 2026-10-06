use silverq::scheduler::batch::Measurement;
use silverq::scheduler::fast_path::{apply_batch, retire_stale};
use silverq::scheduler::node::{Node, HP_INITIAL};
use silverq::scheduler::persist::*;
use std::collections::HashMap;
use std::path::PathBuf;

/// 每个测试独立路径。不碰 SILVERQ_STATE：并行测试共享 env 会互相覆盖。
fn tmp_state(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "silverq-persist-{tag}-{}-{:?}.json",
        std::process::id(),
        std::thread::current().id()
    ))
}

#[test]
fn roundtrip_restores_scores() {
    let path = tmp_state("roundtrip");

    let mut pool = vec![
        Node::new("a", "1.1.1.1", 443),
        Node::new("b", "2.2.2.2", 443),
    ];
    pool[0].update(50.0);
    pool[1].update(300.0);
    save_to(&path, &pool);

    // 新池（分数为 INFINITY）恢复后应拿回原分数
    let mut fresh = vec![
        Node::new("a", "1.1.1.1", 443),
        Node::new("b", "2.2.2.2", 443),
    ];
    assert_eq!(load_from(&path, &mut fresh), 2);
    assert_eq!(fresh[0].score(), 50.0);
    assert_eq!(fresh[1].score(), 300.0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn unmeasured_nodes_are_not_saved() {
    let path = tmp_state("unmeasured");

    let mut pool = vec![
        Node::new("measured", "1.1.1.1", 443),
        Node::new("never", "2.2.2.2", 443),
    ];
    pool[0].update(80.0);
    save_to(&path, &pool);

    let mut fresh = vec![
        Node::new("measured", "1.1.1.1", 443),
        Node::new("never", "2.2.2.2", 443),
    ];
    assert_eq!(load_from(&path, &mut fresh), 1, "只应恢复测过的那个");
    assert_eq!(fresh[0].score(), 80.0);
    assert!(fresh[1].score().is_infinite(), "没测过的应保持 INFINITY");

    let _ = std::fs::remove_file(path);
}

/// 旧格式存档必须整体丢弃，不能把掺了罚分的 ewma 当纯延迟恢复。
///
/// v1 里失败会 `ewma += 3000`（封顶 9999），所以一个真实延迟 1512ms 的
/// 活节点在存档里可能是 7512ms。照原样恢复会让它长期排在后面 ——
/// 宁可从零重测。
#[test]
fn snapshot_from_old_format_is_rejected() {
    let path = tmp_state("oldfmt");

    // v1 存档：无 version 字段（serde default = 0），ewma 是被罚分污染的值
    let v1 = serde_json::json!({
        "saved_at": now_secs(),
        "scores": { "a": { "ewma": 7512.0, "samples": 85 } }
    });
    std::fs::write(&path, serde_json::to_vec(&v1).unwrap()).unwrap();

    let mut pool = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(
        load_from(&path, &mut pool),
        0,
        "旧格式存档必须被拒，否则 7512ms 会被当成真实延迟"
    );
    assert!(pool[0].score().is_infinite());

    // 当前格式则正常恢复
    let mut fresh = vec![Node::new("a", "1.1.1.1", 443)];
    fresh[0].update(1512.0);
    save_to(&path, &fresh);
    let mut pool2 = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(load_from(&path, &mut pool2), 1, "当前格式应能恢复");
    assert_eq!(pool2[0].ewma, 1512.0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn stale_snapshot_is_rejected() {
    let path = tmp_state("stale");

    // 手写一个 7 小时前的存档
    let old = Snapshot {
        version: FORMAT_VERSION,
        saved_at: now_secs() - (7 * 3600),
        scores: HashMap::from([(
            "a".to_string(),
            Score {
                ewma: 42.0,
                samples: 5,
                bw_log: None,
                bw_samples: 0,
                hp: 50,
            },
        )]),
        hp_extra: HashMap::new(),
    };
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

    let mut pool = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(load_from(&path, &mut pool), 0, "过期存档必须被拒");
    assert!(pool[0].score().is_infinite());

    let _ = std::fs::remove_file(path);
}

/// 带宽分数必须跟延迟一起 round-trip：重启后吞吐感知排序不能回退到
/// 「未测过」的乐观初值，否则每轮都要重跑 512KB 下载才能排回真实顺序。
/// 顺带守住 Option<f64> 的 JSON 边界——INFINITY 走 JSON 会变成 null，
/// 序列化/反序列化路径必须被测一次。
#[test]
fn roundtrip_restores_bandwidth() {
    let path = tmp_state("bw");

    let mut pool = vec![
        Node::new("fast", "1.1.1.1", 443),
        Node::new("slow", "2.2.2.2", 443),
    ];
    pool[0].update(120.0);
    pool[1].update(130.0);
    pool[0].update_bw(262_144.0); // 256KB/s
    pool[1].update_bw(16_384.0); // 16KB/s
    save_to(&path, &pool);

    // 全新池（延迟仍是 INFINITY）：恢复只看存档本身，不要求新池先有数据
    let mut fresh = vec![
        Node::new("fast", "1.1.1.1", 443),
        Node::new("slow", "2.2.2.2", 443),
    ];
    assert_eq!(load_from(&path, &mut fresh), 2);

    // 吞吐分数必须回来：快节点 ln 尺度上领先慢节点 ln(16)≈2.77
    let (f, sl) = (fresh[0].bw_bps().unwrap(), fresh[1].bw_bps().unwrap());
    assert!(
        (f - 262_144.0).abs() / 262_144.0 < 0.02,
        "快节点带宽应恢复，got {f}"
    );
    assert!(
        (sl - 16_384.0).abs() / 16_384.0 < 0.02,
        "慢节点带宽应恢复，got {sl}"
    );
    assert!(f > sl * 10.0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn corrupt_and_missing_files_are_tolerated() {
    let path = tmp_state("corrupt");

    // 缺失
    let mut pool = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(load_from(&path, &mut pool), 0);

    // 损坏
    std::fs::write(&path, b"{ not json").unwrap();
    assert_eq!(load_from(&path, &mut pool), 0, "损坏文件不该 panic");
    assert!(pool[0].score().is_infinite());

    let _ = std::fs::remove_file(path);
}

/// 健康度（HP）必须跟延迟/带宽一起 round-trip：重启 6h 内健康度不能
/// 回落到初值 50，否则刚被实际流量惩罚过的节点会立刻拿回「中性」
/// 排序位置，惩罚白做。
#[test]
fn roundtrip_restores_hp() {
    let path = tmp_state("hp");

    let mut pool = vec![Node::new("a", "1.1.1.1", 443)];
    pool[0].update(100.0);
    // 实际流量连续失败 2 次：50 - 30 = 20
    pool[0].note_proxy_failure();
    pool[0].note_proxy_failure();
    save_to(&path, &pool);

    let mut fresh = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(load_from(&path, &mut fresh), 1);
    assert_eq!(fresh[0].hp, 20, "HP 必须随分数存档恢复");
    // 恢复后的 HP 必须立刻参与评分（下行罚分非零）
    assert!(fresh[0].hp_penalty(100.0) > 0.0);

    let _ = std::fs::remove_file(path);
}

/// 旧版本 v3 快照没有 `hp` 字段（新增前线上写出的存档）：读回必须落到
/// 保守初值 50（不奖不罚），而不是 panic 或读成 0。
#[test]
fn legacy_snapshot_without_hp_defaults_to_initial() {
    let path = tmp_state("legacyhp");

    let legacy = serde_json::json!({
        "version": FORMAT_VERSION,
        "saved_at": now_secs(),
        "scores": { "a": { "ewma": 120.0, "samples": 12, "bw_log": null, "bw_samples": 0 } }
    });
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

    let mut pool = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(
        load_from(&path, &mut pool),
        1,
        "缺 hp 字段的当前版本快照仍应可读"
    );
    assert_eq!(pool[0].ewma, 120.0, "延迟照常恢复");
    assert_eq!(pool[0].hp, HP_INITIAL, "缺字段必须读回初值 50");

    let _ = std::fs::remove_file(path);
}

// ── hp_extra：无探测样本节点的健康度/先验存活证据（回归评审 P2）───────

/// 「被实际流量命中过但从未被探测」（`samples==0`）的节点：HP 与
/// `ever_responded` 必须经 `hp_extra` 表存盘并在重启后恢复——否则
/// 5 次探测失败就会被 `retire_stale` 按无证据僵尸立即摘除，尽管它
/// 真实在响应。恢复只写 HP 与证据，不伪造任何探测样本/EWMA。
#[test]
fn roundtrip_restores_hp_extra_for_unprobed_node() {
    let path = tmp_state("hpxtra");

    let mut pool = vec![
        Node::new("live", "1.1.1.1", 443),
        Node::new("plain", "2.2.2.2", 443),
    ];
    // live：从未被探测（samples 保持 0），拿过实际流量首字节，HP 被压下
    pool[0].note_proxy_success();
    pool[0].note_proxy_failure();
    assert_eq!(pool[0].samples, 0);
    assert!(pool[0].ever_responded);
    assert_eq!(pool[0].hp, HP_INITIAL + 2 - 15);
    // plain：基线节点（HP=50 且无证据）——没有可存的东西
    save_to(&path, &pool);

    // 写出的 JSON：hp_extra 表只收偏离节点，主表为空
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let extra = v["hp_extra"].as_object().unwrap();
    assert!(extra.contains_key("live"), "偏离节点必须进 hp_extra");
    assert!(!extra.contains_key("plain"), "基线节点不该存");
    assert!(
        v["scores"].as_object().unwrap().is_empty(),
        "没探测样本进不了主表"
    );

    // 重启：全新池（HP=50、无证据）恢复后拿回 HP 与证据
    let mut fresh = vec![
        Node::new("live", "1.1.1.1", 443),
        Node::new("plain", "2.2.2.2", 443),
    ];
    assert_eq!(load_from(&path, &mut fresh), 1, "主表 0 个 + hp_extra 1 个");
    assert_eq!(
        fresh[0].hp,
        HP_INITIAL + 2 - 15,
        "未探测节点 HP 必须经 hp_extra 恢复"
    );
    assert!(fresh[0].ever_responded, "先验存活证据必须恢复");
    assert_eq!(fresh[0].samples, 0, "证据恢复不得伪造探测样本");
    assert!(fresh[0].score().is_infinite(), "证据恢复不得伪造 EWMA");
    assert_eq!(fresh[1].hp, HP_INITIAL, "未存档的基线节点保持初值");
    assert!(!fresh[1].ever_responded);

    let _ = std::fs::remove_file(path);
}

/// 旧版存档（写于 `hp_extra` 字段之前）没有该表：既有 Score/HP 恢复
/// 行为不变，`ever_responded` 保持 false —— 这类节点仍按无证据处理。
#[test]
fn legacy_snapshot_without_hp_extra_restores_score_hp_not_evidence() {
    let path = tmp_state("legacyhpxtra");

    let legacy = serde_json::json!({
        "version": FORMAT_VERSION,
        "saved_at": now_secs(),
        "scores": { "a": { "ewma": 120.0, "samples": 12, "bw_log": null, "bw_samples": 0, "hp": 61 } }
    });
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

    let mut pool = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(
        load_from(&path, &mut pool),
        1,
        "缺 hp_extra 表的旧存档必须整体可读"
    );
    assert_eq!(pool[0].ewma, 120.0, "延迟照常恢复");
    assert_eq!(pool[0].hp, 61, "Score.hp 恢复语义不变");
    assert!(
        !pool[0].ever_responded,
        "旧存档没有证据 → 仍按无证据节点处理"
    );

    let _ = std::fs::remove_file(path);
}

/// 从未工作过的节点（没探测成功、没拿过流量响应）在存档里不留
/// **证据**：探测失败会扣 HP（5 次 = 50 − 25 = 25），偏离基线的 HP
/// 会落进 `hp_extra`，但 `ever_responded` 必须是 false —— 重启恢复的
/// 只有 HP，没有「存活过」的假象；重新攒满连续失败仍被
/// `retire_stale` 立即摘除。
#[test]
fn unprobed_dead_node_restores_hp_but_not_evidence_and_still_retires() {
    let path = tmp_state("deadtrace");

    let mut pool = vec![Node::new("dead", "9.9.9.9", 443)];
    let dead_probe = || Measurement {
        tag: "dead".into(),
        delay_ms: None,
        bw_bps: None,
    };
    for _ in 0..5 {
        apply_batch(&mut pool, &[dead_probe()]);
    }
    assert!(!pool[0].ever_responded);
    assert_eq!(pool[0].hp, HP_INITIAL - 25, "5 次探测失败扣 25");
    save_to(&path, &pool);

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        v["scores"].as_object().unwrap().is_empty(),
        "无探测样本进不了主表"
    );
    let extra = v["hp_extra"].as_object().unwrap();
    assert_eq!(
        extra
            .get("dead")
            .and_then(|e| e.get("hp"))
            .and_then(|h| h.as_u64()),
        Some((HP_INITIAL - 25) as u64),
        "偏离基线的 HP 照存"
    );
    assert_eq!(
        extra
            .get("dead")
            .and_then(|e| e.get("ever_responded"))
            .and_then(|b| b.as_bool()),
        Some(false),
        "不能凭空造出存活证据"
    );

    // 重启：只拿回 HP，不拿证据；重演 5 次失败 → 立即退役
    let mut fresh = vec![Node::new("dead", "9.9.9.9", 443)];
    assert_eq!(load_from(&path, &mut fresh), 1);
    assert_eq!(fresh[0].hp, HP_INITIAL - 25, "HP 经 hp_extra 恢复");
    assert!(!fresh[0].ever_responded, "证据必须保持 false");
    assert_eq!(fresh[0].samples, 0, "不得伪造探测样本");
    for _ in 0..5 {
        apply_batch(&mut fresh, &[dead_probe()]);
    }
    let retired = retire_stale(&mut fresh, 5, std::time::Duration::from_secs(3600), 0);
    assert_eq!(
        retired,
        vec!["dead".to_string()],
        "不工作节点重启后仍可退役"
    );

    let _ = std::fs::remove_file(path);
}
