//! Fast path: apply measurement results immediately without waiting for the entire pool.
//! Timeout results are heavily penalized and the node is moved toward the back of observation.
use crate::scheduler::batch::Measurement;
use crate::scheduler::node::Node;

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
            // 死节点池里后者是绝大多数，面板上必须分开显示。
            node.last_measured = Some(std::time::Instant::now());

            if let Some(delay) = m.delay_ms {
                node.update(delay);
            } else {
                // 失败只累计次数，罚分在 Node::score() 里现算。
                //
                // 早先是 `ewma += timeout_penalty`，把罚分混进延迟字段：
                // 面板显示 7512ms 像是 2500ms 超时失效（实为 1512ms + 两次罚分），
                // 且罚分是加法、恢复靠 alpha 混合（≤0.65），涨得比恢复快 ——
                // 偶尔失败的活节点被永久压住甚至撞 9999 封顶，与真死节点无法区分。
                node.penalize();
            }
        }
    }
}
