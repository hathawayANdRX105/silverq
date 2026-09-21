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
use crate::scheduler::node::Node;
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
/// v2 的 `ewma` 干净了，但没有带宽分数；v3 加上 `bw_log`/`bw_samples`。
/// 读取非当前版本一律丢弃重测 —— 把 v1 的 7512ms 当真实延迟恢复，
/// 会让一个 1.5s 的活节点长期排在后面。
pub const FORMAT_VERSION: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// 格式版本。缺失 = v1（罚分污染 ewma 的旧格式），丢弃。
    #[serde(default)]
    pub version: u32,
    /// Unix 秒。用于判断存档是否过期。
    pub saved_at: u64,
    /// tag -> 分数
    pub scores: HashMap<String, Score>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Score {
    pub ewma: f64,
    pub samples: u32,
    /// ln(bytes/s)。None = 从未测过吞吐（不恢复带宽分数，保持乐观初值）。
    /// 用 Option 而非 f64：JSON 里 INFINITY 会写成 null、读回来变成 NaN，
    /// Option 让「未测过」有明确表示，不靠特殊浮点值约定。
    pub bw_log: Option<f64>,
    pub bw_samples: u32,
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

pub fn now_secs() -> u64 {
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
                        bw_log: if n.bw_samples > 0 && n.bw_log.is_finite() {
                            Some(n.bw_log)
                        } else {
                            None
                        },
                        bw_samples: n.bw_samples,
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
                if let Some(bw_log) = s.bw_log {
                    n.restore_bw(bw_log, s.bw_samples);
                }
                restored += 1;
            }
        }
    }
    restored
}
