//! `src/scheduler/jev.rs` 的测试：wire 往返、fail-closed 校验、回退触发、
//! 队首应用与并发门。全部走 127.0.0.1 随机端口，不依赖外网。

use serde_json::json;
use silverq::config::settings::JevSection;
use silverq::scheduler::jev::{
    apply_head_to, validate_choice_answer, Candidate, JevDecider, JevHead,
};
use silverq::scheduler::node::Node;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

fn measured_node(tag: &str, server: &str, delay_ms: f64) -> Node {
    let mut n = Node::new(tag, server, 443);
    n.update(delay_ms);
    n.record_outcome(true);
    n
}

/// 两个真实构造的候选（server 是「不得出境」的敏感值）。
fn candidates() -> Vec<Candidate> {
    let a = measured_node("AD-86", "10.9.8.7", 50.0);
    let b = measured_node("BA-1955", "10.9.8.8", 80.0);
    vec![
        Candidate::from_node(&a, 1, 2, 3000.0, 1500.0),
        Candidate::from_node(&b, 2, 2, 3000.0, 1500.0),
    ]
}

/// compatible provider 指向回环 mock 的测试配置。
fn section(base_url: &str) -> JevSection {
    JevSection {
        enabled: true,
        provider: "compatible".into(),
        base_url: base_url.into(),
        api_key: "test-key".into(),
        ..Default::default()
    }
}

fn decider(base_url: &str) -> Arc<JevDecider> {
    JevDecider::new(&section(base_url)).unwrap().unwrap()
}

/// 合法 recommendation：`choice` 拿 p，其余键平分 1-p
/// （键集 = `node*` 候选 + 3 个逃生舱，总和恰为 1）。
fn choice_response(candidates: usize, choice: &str, p: f64, confidence: f64) -> String {
    let mut keys: Vec<String> = (0..candidates).map(|i| format!("node{i}")).collect();
    keys.extend(["ask_user", "investigate", "none"].map(String::from));
    let rest = (1.0 - p) / (keys.len() - 1) as f64;
    let probs: Vec<String> = keys
        .iter()
        .map(|k| {
            let v = if k == choice { p } else { rest };
            format!("\"{k}\":{v}")
        })
        .collect();
    format!(
        r#"{{"answers":{{"recommendation":{{"type":"choice","choice":"{choice}","confidence":{confidence},"probabilities":{{{}}}}}}}}}"#,
        probs.join(",")
    )
}

struct Mock {
    base_url: String,
    /// 请求已读到（响应用于 gate 场景可能还没发）
    arrived: oneshot::Receiver<()>,
    /// 完整请求体（响应写完后送达）
    body: oneshot::Receiver<String>,
}

/// 一次性回环 HTTP 服务。`gate = Some` 时读完请求后等 gate 放行才回响应。
async fn mock_server(status: String, body: String, gate: Option<oneshot::Receiver<()>>) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (arrived_tx, arrived) = oneshot::channel();
    let (body_tx, body_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 2048];
        let head_end = loop {
            let n = sock.read(&mut tmp).await.unwrap();
            assert!(n > 0, "客户端在请求头之前断开");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
                break p + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let content_len = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < head_end + content_len {
            let n = sock.read(&mut tmp).await.unwrap();
            assert!(n > 0, "客户端在请求体之前断开");
            buf.extend_from_slice(&tmp[..n]);
        }
        let request = String::from_utf8_lossy(&buf[head_end..head_end + content_len]).to_string();
        let _ = arrived_tx.send(());
        if let Some(g) = gate {
            g.await.unwrap();
        }
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        let _ = body_tx.send(request);
    });
    Mock {
        base_url: format!("http://{addr}"),
        arrived,
        body: body_rx,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test]
async fn adopts_winner_from_valid_response() {
    let mock = mock_server(
        "200 OK".into(),
        choice_response(2, "node1", 0.62, 0.8),
        None,
    )
    .await;
    let d = decider(&mock.base_url);
    d.decide_round(candidates(), 7).await;

    let head = d.head().expect("合法响应应产生有效决策");
    assert_eq!(head.tag, "BA-1955");
    assert_eq!(head.decided_at_round, 7);
    assert!((head.probability - 0.62).abs() < 1e-9);
    assert_eq!(d.status().stats.successes, 1);

    let body = mock.body.await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).expect("请求体必须是合法 JSON");
    assert_eq!(v["model"], "jev-latest");
    assert_eq!(v["state"]["candidates"][0]["id"], "node0");
    assert_eq!(v["state"]["candidates"][1]["id"], "node1");
    assert!(v["state"]["evidence"].as_str().unwrap().contains("ewma_ms"));
    // hp 是稳定性的直接证据（Node::new 初值 50）：丢失该字段说明
    // 描述串或口径说明被改坏，模型会退回只看 ewma/失败计数
    let evidence = v["state"]["evidence"].as_str().unwrap();
    assert!(
        evidence.contains("hp=50"),
        "evidence 缺 hp 字段: {evidence}"
    );
    assert!(
        v["state"]["priorities"]
            .as_str()
            .unwrap()
            .contains("health credit (hp)"),
        "priorities 未提及 hp"
    );
    let criteria = v["questions"]["recommendation"]["criteria"]
        .as_object()
        .unwrap();
    assert_eq!(criteria.len(), 5, "2 个候选 + 3 个逃生舱");
    assert!(criteria.contains_key("ask_user"));
    assert!(criteria.contains_key("node1"));
    // 出境范围只有 tag + 指标：节点 server 地址不得出现在任何字段里
    assert!(body.contains("BA-1955"));
    assert!(
        !body.contains("10.9.8.8"),
        "server 地址泄进了请求体: {body}"
    );
}

#[tokio::test]
async fn malformed_distribution_falls_back_to_scores() {
    // 少一个概率键（缺 `none`）：形状非法 → 必须 fail-closed，绝不当语义结果
    let body = r#"{"answers":{"recommendation":{"type":"choice","choice":"node0","confidence":0.9,"probabilities":{"node0":0.7,"node1":0.2,"ask_user":0.1,"investigate":0.0}}}}"#;
    let mock = mock_server("200 OK".into(), body.into(), None).await;
    let d = decider(&mock.base_url);
    d.decide_round(candidates(), 1).await;

    assert!(d.head().is_none(), "非法响应不得产生有效决策");
    let s = d.status().stats;
    assert_eq!(s.fallback_invalid, 1);
    assert_eq!(s.successes, 0);
}

#[tokio::test]
async fn escaped_answer_falls_back_to_scores() {
    let mock = mock_server(
        "200 OK".into(),
        choice_response(2, "ask_user", 0.55, 0.7),
        None,
    )
    .await;
    let d = decider(&mock.base_url);
    d.decide_round(candidates(), 1).await;

    assert!(
        d.head().is_none(),
        "逃生舱语义是「问人/补证据」，守护进程无人可问 → 回退"
    );
    let s = d.status().stats;
    assert_eq!(s.fallback_escaped, 1);
    assert_eq!(s.last_error.as_deref(), Some("escaped:ask_user"));
}

#[tokio::test]
async fn low_probability_falls_back_to_scores() {
    // argmax 成立（0.45 > 0.1375）但分布分散在 5 个键上 → 低于 0.5 阈值
    let mock = mock_server(
        "200 OK".into(),
        choice_response(2, "node0", 0.45, 0.9),
        None,
    )
    .await;
    let d = decider(&mock.base_url);
    d.decide_round(candidates(), 1).await;

    assert!(d.head().is_none(), "概率不足的判断不得改变队首");
    assert_eq!(d.status().stats.fallback_low_probability, 1);
}

#[tokio::test]
async fn http_error_counts_as_transport_failure() {
    let mock = mock_server("401 Unauthorized".into(), "nope".into(), None).await;
    let d = decider(&mock.base_url);
    d.decide_round(candidates(), 1).await;

    assert!(d.head().is_none());
    let s = d.status().stats;
    assert_eq!(s.fallback_transport, 1);
    assert_eq!(s.last_error.as_deref(), Some("http_401"));
    assert_eq!(s.consecutive_failures, 1);
}

#[tokio::test]
async fn repeated_failures_enter_cooldown() {
    // 连接被拒（端口 1）→ 传输失败；fail_threshold=2 后冷却 5 轮
    let cfg = JevSection {
        fail_threshold: 2,
        cooldown_rounds: 5,
        ..section("http://127.0.0.1:1")
    };
    let d = JevDecider::new(&cfg).unwrap().unwrap();
    d.decide_round(candidates(), 3).await;
    d.decide_round(candidates(), 3).await;
    d.decide_round(candidates(), 4).await; // round 4 < 3+5 → 冷却期跳过

    let s = d.status().stats;
    assert_eq!(s.attempts, 2, "冷却期不该再发请求");
    assert_eq!(s.skipped_cooldown, 1);
    assert_eq!(s.fallback_transport, 2);
}

#[tokio::test]
async fn only_one_decision_in_flight() {
    let (gate_tx, gate_rx) = oneshot::channel::<()>();
    let mock = mock_server(
        "200 OK".into(),
        choice_response(2, "node0", 0.9, 0.9),
        Some(gate_rx),
    )
    .await;
    let d = decider(&mock.base_url);

    d.spawn_round(candidates(), 1);
    mock.arrived.await.unwrap(); // 第一次请求已发出、响应用 gate 卡住
    d.spawn_round(candidates(), 1);
    assert_eq!(
        d.status().stats.skipped_in_flight,
        1,
        "在飞时第二次必须直接跳过"
    );
    assert_eq!(d.status().stats.attempts, 1);

    gate_tx.send(()).unwrap();
    let head = wait_for_head(&d).await; // 「响应已写」≠「客户端已处理」，轮询到 head
    assert_eq!(head.tag, "AD-86");
    assert_eq!(d.status().stats.successes, 1);
}

/// 有界等待 head 出现（spawn 路径下响应回到 decider 是异步的，直接读会偶发空）。
async fn wait_for_head(d: &JevDecider) -> JevHead {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(h) = d.head() {
                break h;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("第一次决策应在超时前完成并落 head")
}

#[tokio::test]
async fn single_candidate_skips_without_request() {
    let d = decider("http://127.0.0.1:1");
    let one: Vec<Candidate> = candidates().into_iter().take(1).collect();
    d.decide_round(one, 2).await;

    let s = d.status().stats;
    assert_eq!(s.skipped_no_candidates, 1);
    assert_eq!(s.attempts, 0, "候选不足不该发起请求");
    assert!(d.head().is_none());
}

#[test]
fn apply_head_moves_fresh_winner_to_front() {
    let mut desired = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let head = JevHead {
        tag: "c".into(),
        probability: 0.7,
        decided_at_round: 10,
    };
    assert!(apply_head_to(&mut desired, Some(&head), 10, 1));
    assert_eq!(desired, vec!["c", "a", "b"], "其余候选保持分数顺序");
}

#[test]
fn apply_head_ignores_expired_missing_and_head() {
    let head = JevHead {
        tag: "c".into(),
        probability: 0.7,
        decided_at_round: 10,
    };
    // 过期：跨了 2 个轮末 > ttl 1（典型：长时间 pinned 后解除）
    let mut desired = vec!["a".to_string(), "c".to_string()];
    assert!(!apply_head_to(&mut desired, Some(&head), 12, 1));
    assert_eq!(desired, vec!["a", "c"]);

    // tag 已不在候选里（分数跌出 top-N）→ 保持原顺序
    let mut desired = vec!["a".to_string(), "b".to_string()];
    assert!(!apply_head_to(&mut desired, Some(&head), 10, 1));
    assert_eq!(desired, vec!["a", "b"]);

    // 已在队首 → 无事发生
    let mut desired = vec!["c".to_string(), "a".to_string()];
    assert!(!apply_head_to(&mut desired, Some(&head), 10, 1));
    assert_eq!(desired, vec!["c", "a"]);

    // 无决策（回退态）→ 无事发生
    let mut desired = vec!["a".to_string(), "b".to_string()];
    assert!(!apply_head_to(&mut desired, None, 10, 1));
}

#[test]
fn validate_choice_rejects_non_argmax_and_accepts_clean_answer() {
    let expected = ["node0", "node1"];
    let non_argmax = json!({
        "type": "choice",
        "choice": "node0",
        "confidence": 0.9,
        "probabilities": {"node0": 0.3, "node1": 0.7}
    });
    assert!(
        validate_choice_answer(Some(&non_argmax), &expected).is_none(),
        "choice 不是 argmax 必须被拒"
    );

    let sum_drift = json!({
        "type": "choice",
        "choice": "node1",
        "confidence": 0.9,
        "probabilities": {"node0": 0.5, "node1": 0.4}
    });
    assert!(
        validate_choice_answer(Some(&sum_drift), &expected).is_none(),
        "概率和 0.9 偏离 1 超容差必须被拒"
    );

    let clean = json!({
        "type": "choice",
        "choice": "node1",
        "confidence": 0.9,
        "probabilities": {"node0": 0.25, "node1": 0.75}
    });
    let ok = validate_choice_answer(Some(&clean), &expected).expect("合法回答应通过");
    assert_eq!(ok.choice, "node1");
    assert_eq!(ok.confidence, Some(0.9));
    assert!((ok.probabilities["node1"] - 0.75).abs() < 1e-12);

    // confidence 不在 [0,1] 时丢弃但不拒绝（jev 同款语义）
    let odd_confidence = json!({
        "type": "choice",
        "choice": "node1",
        "confidence": 7,
        "probabilities": {"node0": 0.25, "node1": 0.75}
    });
    let ok = validate_choice_answer(Some(&odd_confidence), &expected)
        .expect("confidence 越界不影响判定");
    assert_eq!(ok.confidence, None);
}
