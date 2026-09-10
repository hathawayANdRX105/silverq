//! EWMA 分数持久化：重启后不必从零重新学习。
//!
//! # 为什么需要
//!
//! 真实节点池（217 个 / 6 并发）跑完一轮约 90s。不持久化的话每次重启都要
//! 重新积累 EWMA，期间只能靠"配置顺序播种"这种盲选，选到死节点的概率很高。
//!
//! # 为什么要判过期
//!
//! 延迟分数会腐败：几小时前测的 50ms 现在可能已经不通了。所以存盘带时间戳，
//! 加载时超过 `MAX_AGE` 一律丢弃，宁可从零测也不要用陈旧数据做决策。
//!
//! # 存什么
//!
//! 只存 `tag -> (ewma, samples)`。不存 `recent` 窗口：它只用于计算自适应
//! alpha，丢了最多是重启后头几次 alpha 偏保守，不影响排序正确性；而存它会
//! 让文件大小和格式复杂度都上一个量级。
use crate::node::Node;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 超过这个年龄的存档一律不采用。
/// 取 6 小时：足够覆盖"重启/升级"这类场景，又不会拿隔夜的数据做决策。
const MAX_AGE_SECS: u64 = 6 * 3600;

/// 存档格式版本。
///
/// v1 的 `ewma` 掺了失败罚分（`ewma += 3000`），不是纯实测延迟；
/// 现在 `ewma` 只由成功测速写入，罚分单独记在 `consecutive_failures`。
/// 读到非当前版本一律丢弃重测 —— 把 v1 的 7512ms 当真实延迟恢复，
/// 会让一个 1.5s 的活节点长期排在后面。
const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    /// 格式版本。缺失 = v1（罚分污染 ewma 的旧格式），丢弃。
    #[serde(default)]
    version: u32,
    /// Unix 秒。用于判断存档是否过期。
    saved_at: u64,
    /// tag -> 分数
    scores: HashMap<String, Score>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Score {
    ewma: f64,
    samples: u32,
}

/// 存档路径（`SILVERQ_STATE` 可覆盖）。
pub fn state_path() -> PathBuf {
    std::env::var("SILVERQ_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(
                std::env::var("HOME").unwrap_or_default() + "/.local/state/silverq/scores.json",
            )
        })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 把当前池的分数写盘（默认路径）。
pub fn save(nodes: &[Node]) {
    save_to(&state_path(), nodes);
}

/// 写到指定路径。IO 失败只告警：持久化是优化，不该让调度挂掉。
///
/// 路径显式传入而非从环境变量读：测试并行跑时共享进程 env 会互相覆盖
/// （曾因此必现失败），所以隔离靠参数，不靠 env。
pub fn save_to(path: &std::path::Path, nodes: &[Node]) {
    let snapshot = Snapshot {
        version: FORMAT_VERSION,
        saved_at: now_secs(),
        scores: nodes
            .iter()
            // 没测过的节点没意义，不存
            .filter(|n| n.samples > 0 && n.ewma.is_finite())
            .map(|n| {
                (
                    n.tag.clone(),
                    Score {
                        ewma: n.ewma,
                        samples: n.samples,
                    },
                )
            })
            .collect(),
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("状态目录创建失败: {e}");
            return;
        }
    }

    // 原子写：先写临时文件再 rename，避免进程被杀时留下半个文件
    let tmp = path.with_extension("json.tmp");
    match serde_json::to_vec_pretty(&snapshot) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&tmp, bytes) {
                tracing::warn!("状态写入失败: {e}");
                return;
            }
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::warn!("状态 rename 失败: {e}");
            }
        }
        Err(e) => tracing::warn!("状态序列化失败: {e}"),
    }
}

/// 从默认路径恢复分数。
#[allow(dead_code)] // 生产入口用 load_from(显式路径)；保留默认路径版给嵌入式调用
pub fn load_into(nodes: &mut [Node]) -> usize {
    load_from(&state_path(), nodes)
}

/// 从指定路径恢复分数到池里。返回恢复了多少个节点。
///
/// 存档缺失、损坏、过期都只是"没恢复"，不是错误 —— 从零测一样能工作。
pub fn load_from(path: &std::path::Path, nodes: &mut [Node]) -> usize {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let snapshot: Snapshot = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("状态文件损坏，忽略: {e}");
            return 0;
        }
    };

    if snapshot.version != FORMAT_VERSION {
        tracing::info!(
            found = snapshot.version,
            expected = FORMAT_VERSION,
            "存档格式版本不符（旧格式的 ewma 掺了罚分），丢弃重测"
        );
        return 0;
    }

    let age = now_secs().saturating_sub(snapshot.saved_at);
    if age > MAX_AGE_SECS {
        tracing::info!(age_secs = age, "存档过期（>{MAX_AGE_SECS}s），从零开始测速");
        return 0;
    }

    let mut restored = 0;
    for n in nodes.iter_mut() {
        if let Some(s) = snapshot.scores.get(&n.tag) {
            if s.ewma.is_finite() && s.samples > 0 {
                n.restore_score(s.ewma, s.samples);
                restored += 1;
            }
        }
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
