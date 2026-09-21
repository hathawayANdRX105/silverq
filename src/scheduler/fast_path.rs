//! Fast path: apply measurement results immediately without waiting for the entire pool.
//! Timeout results are heavily penalized and the node is moved toward the back of observation.
use crate::scheduler::batch::Measurement;
use crate::scheduler::node::Node;
use std::time::Duration;

/// 把一批测速结果写回节点池：成功的更新 EWMA，超时/失败的扣分后移。
///
/// 扣分只对曾经成功过的节点有意义。从未成功的节点 `ewma` 仍是 `INFINITY`，
/// 本来就排在最后。真正需要扣分的是先活后死的节点：否则一个刚测出 50ms 的
/// 节点挂掉后，仍会带着 50ms 的分数长期霸占队首。
pub fn apply_batch(nodes: &mut [Node], measurements: &[Measurement]) {
    for m in measurements {
        if let Some(node) = nodes.iter_mut().find(|n| n.tag == m.tag) {
            // 记录"测过了"，无论成败。samples 只在成功时才 +1，所以
            // samples==0 无法区分「还没轮到」和「测了但全失败」——
            node.last_measured = Some(std::time::Instant::now());
            // 延迟成功才更新 EWMA；失败只累计次数，罚分在 score() 里现算。
            //
            // 早先是 `ewma += timeout_penalty`，把罚分混进延迟字段：面板显示
            // 7512ms 像是 2500ms 超时失效（实为 1512ms + 两次罚分），且罚分是
            // 加法、恢复靠 alpha 混合（≤0.65），涨得比恢复快——偶尔失败的活节点
            // 被永久压住甚至撞 9999 封顶，与真死节点无法区分。
            if let Some(delay) = m.delay_ms {
                node.update(delay);
            } else {
                node.penalize();
            }
            // 探测成败入稳定性窗口（无论成败）。
            // 只在失败时记会让「两次挂一次」的间歇劣化节点看起来完全健康。
            // 吞吐批 delay_ms 恒为 None，成败看 bw_bps——否则成功的吞吐探测
            // 会被记成失败，把活节点的稳定性乘子压低。
            let ok = m.delay_ms.is_some() || m.bw_bps.is_some();
            node.record_outcome(ok);
            // 带宽采样（延迟批里一般为 None，吞吐批才填）。
            if let Some(bps) = m.bw_bps {
                node.update_bw(bps);
            }
        }
    }
}

/// 轮末淘汰 —— 两级 pipeline（次数硬指标 + 时间延长保活）：
///
/// 1. 连续失败 < `max_failures`：正常观察，不淘汰。
/// 2. 连续失败 ≥ `max_failures` 且**从未测通**（samples==0）：立即摘除——
///    密码错、协议死、被墙死都不会自愈，不存在"临时故障"，无保命价值。
/// 3. 连续失败 ≥ `max_failures` 且**曾测通过**（samples>0）：延长保活——
///    可能是临时故障，继续观察，直到连续失败持续满 `keep_alive` 才摘。
///    期间测速成功一次即复活（consecutive_failures 清零、计时重置）。
///
/// `max_failures == 0` 禁用。`keep_alive == 0` 表示曾通过的也立即摘。
/// 返回被摘的 tag 列表（日志用）。
pub fn retire_stale(
    nodes: &mut Vec<Node>,
    max_failures: u32,
    keep_alive: Duration,
    min_pool: usize,
) -> Vec<String> {
    if max_failures == 0 {
        return Vec::new();
    }
    let (mut kept, mut retired): (Vec<_>, Vec<_>) = nodes.drain(..).partition(|n| {
        if n.consecutive_failures < max_failures {
            return true; // 保留
        }
        match (n.samples, n.failing_since) {
            (0, _) => false,                            // 从未测通的僵尸，立即摘
            (_, None) => true, // 不应发生（failures>0 必有 failing_since），保守保留
            (_, Some(t0)) => t0.elapsed() < keep_alive, // 曾通过：保活期内保留
        }
    });
    // 地板保护：摘到低于 min_pool 时回填最可能活的（失败次数少 → samples 多）。
    // 留着不摘的节点每轮仍会被探测，成功一次即复活；摘了只能等 reload 重灌。
    if kept.len() < min_pool && !retired.is_empty() {
        retired.sort_by_key(|n| (n.consecutive_failures, std::cmp::Reverse(n.samples)));
        let need = (min_pool - kept.len()).min(retired.len());
        kept.extend(retired.drain(..need));
    }
    *nodes = kept;
    retired.into_iter().map(|n| n.tag).collect()
}
