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
//! 存 `tag -> (ewma, samples, 带宽分数, 健康度 hp)`。没探测样本
//! （`samples==0`）的节点没有 EWMA 可存，但若它被实际流量命中过
//! （HP 偏离初值或 `ever_responded`），在 `hp_extra` 表里补记健康度
//! 与先验存活证据——不记的话重启后 HP 回落 50、证据丢失，5 次探测
//! 失败即被 `retire_stale` 立即摘除，尽管它真实在响应。不存
//! `recent` 窗口：它只用于计算自适应 alpha，丢了最多是重启后头几次
//! alpha 偏保守，不影响排序正确性；而存它会让文件大小和格式复杂度
//! 都上一个量级。hp 与 EWMA 同寿命（6h 新鲜度）：长期停机重启后
//! 健康度回落保守初值 50，证据回到「无」。
use crate::scheduler::node::{Node, HP_INITIAL};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 超过这个年龄的存档一律不采用。
/// 取 6 小时：足够覆盖"重启/升级"这类场景，又不会拿隔夜的数据做决策。
const MAX_AGE_SECS: u64 = 6 * 3600;

/// 存档格式版本。
///
/// v2 的 `ewma` 干净了，但没有带宽分数；v3 加上 `bw_log`/`bw_samples`
/// 与健康度 `hp`（serde 默认 50，旧 v3 快照缺字段时读回保守初值）。
/// `hp_extra`（无探测样本节点的健康度/证据）是 v3 内的加性字段：
/// serde 默认空 map，旧 v3 快照没有该字段照样可读，主 `scores` 不受影响。
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
    /// 没探测样本（`samples==0`）节点的补记表：`tag -> (hp, ever_responded)`。
    /// 主 `scores` 不存这类节点（没有 EWMA 可存），但「被实际流量命中过」
    /// 的节点 HP 可能偏离初值、且带 `retire_stale` 保活决策依据
    /// `ever_responded`——不存则重启后证据丢失，5 次探测失败即被摘除。
    /// 旧快照没有该字段 → serde 默认空 map，不影响主 `scores` 恢复。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub hp_extra: HashMap<String, HpExtra>,
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
    /// 节点健康度（0..=100，初始 50）。与 EWMA 同寿命：随分数存档持久化，
    /// 老版本 v3 快照没有该字段 → 读时回落到 50（保守初值，不奖不罚）。
    #[serde(default = "default_hp")]
    pub hp: u8,
}

fn default_hp() -> u8 {
    HP_INITIAL
}

/// `Snapshot.hp_extra` 里无探测样本节点的补记。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HpExtra {
    /// 节点健康度（0..=100，初始 50），与 EWMA 同寿命（同 6h 新鲜度窗口）。
    pub hp: u8,
    /// 该节点拿过实际流量响应（代理首字节）——先验存活证据。
    pub ever_responded: bool,
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
    let scores: HashMap<String, Score> = nodes
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
                    hp: n.hp,
                },
            )
        })
        .collect();
    // 无探测样本节点的补记：仅当 HP 偏离初值或拿过实际流量响应证据时
    // 才存——全基线节点（HP=50 且无证据）千篇一律，存了只增大文件。
    let hp_extra: HashMap<String, HpExtra> = nodes
        .iter()
        .filter(|n| n.samples == 0 && (n.hp != HP_INITIAL || n.ever_responded))
        .map(|n| {
            (
                n.tag.clone(),
                HpExtra {
                    hp: n.hp,
                    ever_responded: n.ever_responded,
                },
            )
        })
        .collect();
    let snapshot = Snapshot {
        version: FORMAT_VERSION,
        saved_at: now_secs(),
        scores,
        hp_extra,
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
                n.restore_hp(s.hp);
                if let Some(bw_log) = s.bw_log {
                    n.restore_bw(bw_log, s.bw_samples);
                }
                restored += 1;
            }
        }
    }
    // `hp_extra` 独立于主表恢复：只给池里 tag 匹配的节点应用（没测过
    // 样本的节点不在 `scores` 里，靠这张表拿回 HP 与先验存活证据；
    // 旧快照没有该表 → 自然跳过）。与主表同受上方 6h 年龄上限约束，
    // 不写任何假的探测样本/EWMA。
    for (tag, extra) in snapshot.hp_extra.iter() {
        if let Some(n) = nodes.iter_mut().find(|n| &n.tag == tag) {
            n.restore_hp_extra(extra.hp, extra.ever_responded);
            restored += 1;
        }
    }
    restored
}
