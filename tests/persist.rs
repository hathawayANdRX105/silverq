use silverq::scheduler::node::Node;
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
            },
        )]),
    };
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

    let mut pool = vec![Node::new("a", "1.1.1.1", 443)];
    assert_eq!(load_from(&path, &mut pool), 0, "过期存档必须被拒");
    assert!(pool[0].score().is_infinite());

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
