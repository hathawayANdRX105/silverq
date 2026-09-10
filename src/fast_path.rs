//! Fast path: apply measurement results immediately without waiting for the entire pool.
//! Timeout results are heavily penalized and the node is moved toward the back of observation.
use crate::batch::Measurement;
use crate::node::Node;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn node(tag: &str) -> Node {
        Node::new(tag, "127.0.0.1", 443)
    }

    fn ok(tag: &str, ms: f64) -> Measurement {
        Measurement {
            tag: tag.into(),
            delay_ms: Some(ms),
        }
    }

    fn timeout(tag: &str) -> Measurement {
        Measurement {
            tag: tag.into(),
            delay_ms: None,
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

        // 一次成功立刻完全恢复 —— 不需要多轮把膨胀分数洗回来
        apply_batch(&mut pool, &[ok("flaky", 100.0)]);
        assert_eq!(
            pool[0].score(),
            pool[0].ewma,
            "一次成功后排序分数必须等于实测延迟，score={} ewma={}",
            pool[0].score(),
            pool[0].ewma
        );
        assert_eq!(pool[0].consecutive_failures, 0);
    }
}
