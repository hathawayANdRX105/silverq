//! Fast path: apply measurement results immediately without waiting for the entire pool.
//! Timeout results are heavily penalized and the node is moved toward the back of observation.
use crate::scheduler::batch::Measurement;
use crate::scheduler::node::Node;
use std::time::Duration;

/// 把一批测速结果写回节点池：成功的更新 EWMA/带宽并抬健康度，
/// 超时/失败扣失败计数并压低健康度。
///
/// 健康度（`Node::hp`）在这里按「每次探测恰好一次」记账：
/// 成功 +1（延迟成功或带宽成功，二者共存时也只记一次），失败 -5。
/// 带宽采样值本身不重复加分——`update_bw` 只管 EWMA 与清失败计数。
///
/// 根因注意：带宽成功（`delay_ms=None, bw_bps=Some`）是节点存活的证据，
/// 绝不能走 `penalize()`——早先实现里它被误计失败，`consecutive_failures`
/// 越攒越高，`retire_stale` 凭旧的 `failing_since` 把活节点摘掉
/// （`first_choice` 从 fast 掉到 backup 的回归）。现在该路径只走
/// `update_bw`（清失败计数）+ 健康度 +1。
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
            let success = m.delay_ms.is_some() || m.bw_bps.is_some();
            // 延迟成功更新 EWMA（顺带清失败计数，见 Node::update）；
            // 带宽成功更新对数域带宽 EWMA（顺带清失败计数，见 Node::update_bw）。
            //
            // 早先是 `ewma += timeout_penalty`，把罚分混进延迟字段：面板显示
            // 7512ms 像是 2500ms 超时失效（实为 1512ms + 两次罚分），且罚分是
            // 加法、恢复靠 alpha 混合（≤0.65），涨得比恢复快——偶尔失败的活节点
            // 被永久压住甚至撞 9999 封顶，与真死节点无法区分。
            if let Some(delay) = m.delay_ms {
                node.update(delay);
                if let Some(bps) = m.bw_bps {
                    node.update_bw(bps);
                }
                node.note_probe_success();
            } else if success {
                // 带宽成功（delay 无、bw 有）：存活证据，不计失败
                if let Some(bps) = m.bw_bps {
                    node.update_bw(bps);
                }
                node.note_probe_success();
            } else {
                node.penalize();
                node.note_probe_failure();
            }
            // 探测成败入稳定性窗口（无论成败）。
            // 只在失败时记会让「两次挂一次」的间歇劣化节点看起来完全健康。
            node.record_outcome(success);
        }
    }
}

/// 轮末淘汰 —— 次数硬指标 + 时间延长保活（按存活证据分三类）：
///
/// 1. 连续失败 < `max_failures`：正常观察，不淘汰。
/// 2. 连续失败 ≥ `max_failures` 且**无任何存活证据**（samples==0 且
///    `ever_responded==false`，从未探测成功、从未拿过实际流量首字节）：
///    延长保活——连续失败持续满 `keep_alive` 才摘。
/// 3. 连续失败 ≥ `max_failures` 且**有存活证据**（曾测通过，或实际流量
///    曾响应）：同样延长保活，直到连续失败持续满 `keep_alive` 才摘。
///    期间测速成功一次即复活（consecutive_failures 清零、计时重置）。
///
/// 第 2、3 类都走保活期是有意的：节点服务器域名解析失败、探测端点对该
/// 节点返回 403、上游临时抽风——这些都会让一批节点同时攒满
/// `max_failures`。早先「无存活证据立即摘」把这种批量瞬时故障当成僵尸
/// 判死：实测一轮摘掉 437~508 个（占池 90%+），且把用户自有的精选节点
/// 一并摘光。摘掉的节点只能等 15min 后的 reload 重灌，重灌后又被
/// `adopt_score` 带回旧的失败计数，于是「reload → 批量误杀 → 再 reload」
/// 成环，池子永远在换血。
///
/// 第 3 类的「实际流量曾响应」证据是 `ever_responded`（`note_proxy_success`
/// 置位，不靠写假探测样本/EWMA）：一个「被流量命中过但从未被探测」
/// （samples==0）的节点，若重启丢了这份证据、再攒满 5 次连续探测失败
/// 就会被按无证据僵尸摘除，尽管它真实在响应——该证据随
/// `hp_extra` 存档、随 `adopt_score` 迁移，就是为了保住它。
///
/// `max_failures == 0` 禁用。`keep_alive == 0` 表示立即摘。
/// `max_retire_per_round` 是单轮批量上限（占池子比例），防止一次
/// 全池网络抖动把池子摘空到无法服务；超出的部分留到下一轮再摘。
/// 返回被摘的 tag 列表（日志用）。
pub fn retire_stale(
    nodes: &mut Vec<Node>,
    max_failures: u32,
    keep_alive: Duration,
    min_pool: usize,
    max_retire_per_round: f64,
) -> Vec<String> {
    if max_failures == 0 {
        return Vec::new();
    }
    let total = nodes.len();
    let (mut kept, mut retired): (Vec<_>, Vec<_>) = nodes.drain(..).partition(|n| {
        if n.consecutive_failures < max_failures {
            return true; // 保留
        }
        match (n.samples, n.ever_responded, n.failing_since) {
            (_, _, None) => true, // 不应发生（failures>0 必有 failing_since），保守保留
            // 有存活证据（曾测通过 / 实际流量曾响应）：保活期内保留
            (_, _, Some(t0)) => t0.elapsed() < keep_alive,
        }
    });
    // 单轮批量上限：一次全池抖动不该把池子一次摘空。超出上限的部分本轮
    // 留下（下一轮继续观察），按「失败次数少 → samples 多」排序，
    // 优先摘最像死节点的。
    if max_retire_per_round > 0.0 && max_retire_per_round < 1.0 && !retired.is_empty() {
        let cap = ((total as f64) * max_retire_per_round).floor() as usize;
        if retired.len() > cap {
            retired.sort_by_key(|n| (n.consecutive_failures, std::cmp::Reverse(n.samples)));
            let overflow = retired.split_off(cap);
            kept.extend(overflow);
        }
    }
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
