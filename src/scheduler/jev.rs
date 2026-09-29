//! jev 判断接入：每轮测速结束后，对「分数前 N」候选做一次 bounded decision，
//! 采纳时只把胜出节点顶到 selection 队首；**任何失败都回退纯分数选择**。
//!
//! # 协议
//!
//! 复刻 `@jkudish/jev-mcp` 0.11.0 的 `jev_decide`（所有 provider 同形）：
//!
//! ```text
//! POST <endpoint>   Authorization: Bearer <key>
//! {"model": …,
//!  "state": {decision, evidence, priorities, candidates, requirements},
//!  "questions": {"recommendation": {"type": "choice", "instructions": …, "criteria": {…}}}}
//! ← {"answers": {"recommendation": {"type": "choice", "choice": …,
//!                                   "probabilities": {…}, "confidence": …}}, "usage": …}
//! ```
//!
//! 升级 jev 时对照包内 `dist/index.js` 的 `jev_decide` 分支校验本模块的
//! 构造与校验是否漂移。
//!
//! # 边界与回退语义
//!
//! - **jev 只影响队首是谁**：胜出节点挪到 `desired[0]`，其余候选仍按分数排。
//!   数据面 best→次优 候选链原样保留，是判断选错时的第二道保险。
//! - 回退触发（任一命中即回纯分数选择，且作废已有决策）：
//!   传输失败（超时/网络/非 2xx——端点不健康，不是判断问题）、
//!   非法响应（`invalid_response`，fail-closed）、逃生舱
//!   （`ask_user`/`investigate`/`none`）、胜出概率低于 `min_probability`。
//!   连续失败达阈值后进入冷却，冷却期不发请求（不每轮白等超时）。
//! - **pinned 时既不发起也不应用**：手动钉住优先级最高（见 `reselect`）。
//! - evidence 只含 tag 与纯指标：**不发 server/端口/凭证**，节点拓扑不外泄。
//! - wire id 用 `node0..`（jev 要求 `^[a-z][a-z0-9_-]*$`，真实 tag 如 `AD-86`
//!   不合法）；tag 只进 description，返回的 selected 按序号映射回 tag。

use crate::config::settings::JevSection;
use crate::scheduler::node::Node;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// `jev_decide` 单次决策的候选上限（协议硬限制 2..=6）。
pub const MAX_CANDIDATES: usize = 6;

/// 概率和校验容差（jev 侧 `PROBABILITY_SUM_TOLERANCE`）：上游分布按两位小数
/// 上报，k 个非零项最多漂 0.005×k，故给 1%~5% 的带子，垃圾值仍会被拒。
const PROBABILITY_SUM_TOLERANCE: f64 = 0.01 + 1e-12;
/// 胜出项必须是 argmax 的容差（jev 侧 1e-9，防 IEEE-754 比较噪声）。
const ARGMAX_EPSILON: f64 = 1e-9;

/// 逃生舱（jev-mcp `DECIDE_ESCAPE_HATCHES` 原文）。语义进 prompt，勿改写。
const ESCAPE_HATCHES: [(&str, &str); 3] = [
    (
        "ask_user",
        "A consequential user preference or requirement is missing; ask instead of inventing it",
    ),
    (
        "investigate",
        "Gather missing technical or factual evidence before selecting a candidate",
    ),
    (
        "none",
        "None of the supplied candidates fits the known requirements",
    ),
];

/// recommendation 题干（jev-mcp `jev_decide` 原文）。
const RECOMMENDATION_INSTRUCTIONS: &str = "Which candidate best fits the decision, evidence, and priorities? Select a candidate or an escape hatch. Do not invent missing facts, preferences, or approvals.";

/// 有界决策问题本身（≤1500 字符，jev 侧长度上限）。
const DECISION_TEXT: &str = "Choose which proxy node should lead silverq's selection — be tried first for all new connections — for the next measurement round.";

/// 显式优先级（≤2000 字符）。先讲口径，免得模型只盯 rank 或只盯延迟。
const PRIORITIES_TEXT: &str = "1) Probe stability first: fewer consecutive failures and a higher recent success rate beat raw latency. 2) Then lower recent latency (ewma_ms). 3) Prefer freshly probed nodes over stale ones. 4) Adequate throughput when measured; unmeasured bandwidth is not a downside. All candidates already passed score-based filtering — pick the one you trust most to carry traffic first; rank alone is not decisive.";

/// 一个候选节点的证据快照（只含 tag 与纯指标，不含 server/端口/凭证）。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 节点 tag。只出现在 description 文本里；wire id 另用 `node{i}`。
    pub tag: String,
    /// 给 provider 看的描述：tag + 指标 + 当前分数名次。
    pub description: String,
}

impl Candidate {
    /// 从节点池条目构造候选。`rank`（1 起）/`total` 是分数名次与候选总数，
    /// 写进描述给判断一个「当前排位」的上下文；分数用与 `select_top`
    /// 完全相同的两个 penalty 参数算，保证描述里的 score 与真实排序同源。
    pub fn from_node(
        n: &Node,
        rank: usize,
        total: usize,
        penalty_ms: f64,
        bw_penalty_per_efold_ms: f64,
    ) -> Self {
        let ewma = if n.ewma.is_finite() {
            format!("{:.0}", n.ewma)
        } else {
            "unmeasured".into()
        };
        let probe_age = n
            .last_measured
            .map(|t| t.elapsed().as_secs().to_string())
            .unwrap_or_else(|| "never".into());
        let bw = n
            .bw_bps()
            .map(|b| format!("{b:.0}"))
            .unwrap_or_else(|| "unmeasured".into());
        Self {
            tag: n.tag.clone(),
            description: format!(
                "tag={} | rank={}/{} | ewma_ms={} | consecutive_failures={} | \
                 stability={:.2} | samples={} | bw_bps={} | probe_age_s={} | score={:.1}",
                n.tag,
                rank,
                total,
                ewma,
                n.consecutive_failures,
                n.stability(),
                n.samples,
                bw,
                probe_age,
                n.score_with(penalty_ms, bw_penalty_per_efold_ms),
            ),
        }
    }
}

/// 一次成功的决策（应用到队首的凭据）。
#[derive(Debug, Clone, Serialize)]
pub struct JevHead {
    /// 胜出节点 tag
    pub tag: String,
    /// 队列分布里该候选的概率
    pub probability: f64,
    /// 决策时的轮计数（配合 `ttl_rounds` 判断过期）
    pub decided_at_round: u64,
}

/// 回退原因分类（进 stats 计数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    Transport,
    Invalid,
    Escaped,
    LowProbability,
}

/// 决策运行统计：web 面板 / ctl status / 日志共用一份口径。
#[derive(Debug, Default, Clone, Serialize)]
pub struct JevStats {
    /// 发起过的决策次数（不含冷却/在飞/候选不足的跳过）
    pub attempts: u64,
    /// 采纳次数（写入有效 head）
    pub successes: u64,
    pub fallback_transport: u64,
    pub fallback_invalid: u64,
    pub fallback_escaped: u64,
    pub fallback_low_probability: u64,
    pub skipped_cooldown: u64,
    pub skipped_in_flight: u64,
    pub skipped_no_candidates: u64,
    /// 连续「拿不到可用决策」次数（采纳即清零）
    pub consecutive_failures: u32,
    /// 最近一次失败原因（固定短语，如 `http_401` / `timeout` /
    /// `invalid_response` / `escaped:ask_user`——不含 URL 与密钥）
    pub last_error: Option<String>,
    /// 最近一次采纳的 unix 秒
    pub last_success_ts: u64,
}

impl JevStats {
    fn bump(&mut self, f: Fallback) {
        match f {
            Fallback::Transport => self.fallback_transport += 1,
            Fallback::Invalid => self.fallback_invalid += 1,
            Fallback::Escaped => self.fallback_escaped += 1,
            Fallback::LowProbability => self.fallback_low_probability += 1,
        }
    }

    /// 回退总数（面板与 ctl status 用）。
    pub fn fallback_total(&self) -> u64 {
        self.fallback_transport
            + self.fallback_invalid
            + self.fallback_escaped
            + self.fallback_low_probability
    }
}

/// provider 传输形态。三者都吃 `{model, state, questions}`，差别只在
/// endpoint / 认证头 / 模型写法（cloudflare、vercel 的信封不同，暂不支持）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Typesafe,
    Openrouter,
    Compatible,
}

impl Provider {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "typesafe" => Some(Self::Typesafe),
            "openrouter" => Some(Self::Openrouter),
            "compatible" => Some(Self::Compatible),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Typesafe => "typesafe",
            Self::Openrouter => "openrouter",
            Self::Compatible => "compatible",
        }
    }
}

/// 请求落点（构造期定死，运行期不变）。
#[derive(Debug, Clone)]
struct Endpoint {
    url: String,
    /// 请求体里的 model 字段（openrouter 侧已换成它的 slug）
    model: String,
    bearer: String,
    referer: Option<String>,
    title: Option<String>,
}

/// 分类后的单次决策结果。
enum Outcome {
    Selected {
        tag: String,
        probability: f64,
        confidence: Option<f64>,
    },
    Escaped(String),
    Invalid(String),
    Transport(String),
}

/// HTTP 层错误：传输（端点不健康）与「响应体不可解析」分开——后者按
/// jev 的 fail-closed 口径是 `invalid_response`（协议失败，不是判断失败）。
enum AskError {
    Transport(String),
    InvalidBody,
}

/// 校验通过的 choice 回答。
#[derive(Debug)]
pub struct ValidatedChoice {
    pub choice: String,
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: Option<f64>,
}

/// jev 侧 `validateChoiceAnswer` 的复刻：**形状与语义同时成立才算数**。
///
/// 任一条不满足返回 `None`——非法响应绝不能当语义结果用（fail-closed）：
/// 键集必须与期望完全一致、概率有限且落在 [0,1]、总和近 1、
/// choice 必须是 argmax。confidence 不是判定条件，只在合法时透出。
pub fn validate_choice_answer(
    answer: Option<&Value>,
    expected: &[&str],
) -> Option<ValidatedChoice> {
    let obj = answer?.as_object()?;
    let choice = obj.get("choice")?.as_str()?.to_string();
    let probs = obj.get("probabilities")?.as_object()?;
    if probs.len() != expected.len() || !expected.iter().all(|k| probs.contains_key(*k)) {
        return None;
    }
    let mut map = BTreeMap::new();
    let mut sum: f64 = 0.0;
    let mut max: f64 = 0.0;
    for key in expected {
        let p = probs.get(*key)?.as_f64()?;
        if !p.is_finite() || !(0.0..=1.0).contains(&p) {
            return None;
        }
        sum += p;
        max = max.max(p);
        map.insert((*key).to_string(), p);
    }
    if (sum - 1.0).abs() > PROBABILITY_SUM_TOLERANCE {
        return None;
    }
    if map.get(&choice)? + ARGMAX_EPSILON < max {
        return None;
    }
    let confidence = obj
        .get("confidence")
        .and_then(Value::as_f64)
        .filter(|c| c.is_finite() && (0.0..=1.0).contains(c));
    Some(ValidatedChoice {
        choice,
        probabilities: map,
        confidence,
    })
}

/// 构造 `jev_decide` 请求体（`{model, state, questions}`）。
/// 公开供测试断言 wire 形状——这是与外部服务的真实契约。
pub fn build_decide_request(candidates: &[Candidate], model: &str) -> Value {
    let mut criteria = Map::new();
    let mut state_candidates = Vec::with_capacity(candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        let id = format!("node{i}");
        state_candidates.push(json!({"id": id, "description": c.description}));
        criteria.insert(id, Value::String(c.description.clone()));
    }
    for (name, desc) in ESCAPE_HATCHES {
        criteria.insert(name.to_string(), Value::String(desc.to_string()));
    }
    json!({
        "model": model,
        "state": {
            "decision": DECISION_TEXT,
            "evidence": build_evidence(candidates),
            "priorities": PRIORITIES_TEXT,
            "candidates": state_candidates,
            "requirements": [],
        },
        "questions": {
            "recommendation": {
                "type": "choice",
                "instructions": RECOMMENDATION_INSTRUCTIONS,
                "criteria": criteria,
            }
        },
    })
}

/// evidence：真实测速指标表 + 评分口径说明（只 tag + 纯指标）。
fn build_evidence(candidates: &[Candidate]) -> String {
    let mut s = String::from(
        "Real probe measurements from silverq's scheduler (facts only, no opinions):\n",
    );
    for (i, c) in candidates.iter().enumerate() {
        s.push_str(&format!("node{i}: {}\n", c.description));
    }
    s.push_str(
        "Scoring context: score = (ewma_latency_ms + consecutive_failures * timeout_penalty_ms) \
         / stability + bandwidth_penalty_ms, lower is better. ewma is written only by successful \
         probes; stability is the success ratio over the last 16 probes; unmeasured bandwidth is \
         not a penalty.",
    );
    s
}

/// openrouter 的模型 slug：它没有 `jev-latest` 别名，只认具体版本
/// （`jev-latest`→当前版本的映射与 jev-mcp 0.11.0 保持一致；用
/// `[jev].model` 显式指定即可覆盖）。
fn openrouter_slug(model: &str) -> String {
    if model.starts_with("typesafe/") {
        return model.to_string();
    }
    match model {
        "jev-latest" => "typesafe/jev-1.13".into(),
        other => format!("typesafe/{other}"),
    }
}

fn resolve_api_key(section: &JevSection, provider: Provider) -> Option<String> {
    if !section.api_key.trim().is_empty() {
        return Some(section.api_key.trim().to_string());
    }
    let env = match provider {
        Provider::Typesafe => "TYPESAFE_API_KEY",
        Provider::Openrouter => "OPENROUTER_API_KEY",
        Provider::Compatible => "JEV_API_KEY",
    };
    std::env::var(env).ok().filter(|s| !s.is_empty())
}

/// 决策器：持有 endpoint、HTTP client 与唯一一份「head + 统计」状态。
///
/// head 与统计放同一个 mutex：`reselect` 每批读一次 head（无竞争、开销可忽略），
/// 写只发生在轮末的单个决策任务里。
pub struct JevDecider {
    provider: &'static str,
    endpoint: Endpoint,
    /// 送审候选数（配置夹到 2..=MAX_CANDIDATES）
    candidate_count: usize,
    min_probability: f64,
    ttl_rounds: u64,
    fail_threshold: u32,
    cooldown_rounds: u64,
    http: reqwest::Client,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    head: Option<JevHead>,
    stats: JevStats,
    in_flight: bool,
    cooldown_until_round: u64,
}

/// 决策任务的在飞标记守卫：panic 也要复位，否则一次意外就永久停摆。
struct InFlightGuard(Arc<JevDecider>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.inner.lock().in_flight = false;
    }
}

impl JevDecider {
    /// 构造决策器。
    ///
    /// - 未启用 → `Ok(None)`（常态，零开销）
    /// - 启用且配置可用 → `Ok(Some(..))`
    /// - 启用但配置残缺 → `Err(原因)`：调用方只降级禁用，不杀启动；
    ///   原因是固定短语，不含 key。
    pub fn new(section: &JevSection) -> Result<Option<Arc<Self>>, String> {
        if !section.enabled {
            return Ok(None);
        }
        let provider = Provider::parse(&section.provider).ok_or_else(|| {
            format!(
                "未知 provider `{}`（可选 typesafe/openrouter/compatible）",
                section.provider
            )
        })?;
        let bearer = resolve_api_key(section, provider)
            .ok_or("api_key 未配置（[jev].api_key 或 provider 标准环境变量）")?;

        let base = section.base_url.trim();
        if provider == Provider::Compatible && base.is_empty() {
            return Err("compatible provider 必须配置 base_url".into());
        }
        let model = if section.model.trim().is_empty() {
            "jev-latest"
        } else {
            section.model.trim()
        };
        let (url, model) = match provider {
            Provider::Typesafe => {
                let base = if base.is_empty() {
                    "https://api.typesafe.ai"
                } else {
                    base
                };
                (
                    format!("{}/v1/systemone", base.trim_end_matches('/')),
                    model.to_string(),
                )
            }
            Provider::Openrouter => (
                "https://openrouter.ai/api/alpha/decisions".to_string(),
                openrouter_slug(model),
            ),
            Provider::Compatible => (base.to_string(), model.to_string()),
        };

        let timeout = Duration::from_secs(section.timeout_secs.clamp(2, 60));
        // no_proxy：判断请求是基础设施流量，不吃 HTTP(S)_PROXY 环境变量——
        // 环境代理若指向 silverq 自己会形成自指递归（dial 侧 2026-09-19 同款坑），
        // 需要走代理就在 provider 端点上表达。
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .user_agent(format!("silverq/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("http client 初始化失败: {e}"))?;

        Ok(Some(Arc::new(Self {
            provider: provider.name(),
            endpoint: Endpoint {
                url,
                model,
                bearer,
                referer: Some("https://github.com/jkudish/jev-mcp".into()),
                title: Some("silverq-jev".into()),
            },
            candidate_count: section.candidate_count.clamp(2, MAX_CANDIDATES),
            min_probability: section.min_probability.clamp(0.0, 1.0),
            ttl_rounds: section.ttl_rounds,
            fail_threshold: section.fail_threshold.max(1),
            cooldown_rounds: section.cooldown_rounds,
            http,
            inner: Mutex::new(Inner::default()),
        })))
    }

    /// 轮末发起一次决策：单在飞限制（上一次没回来就跳过本轮），
    /// 内部 `tokio::spawn` 立即返回——测速循环不能被 jev 的超时拖住
    /// （测速与切换解耦的卡顿隔离原则）。
    pub fn spawn_round(self: &Arc<Self>, candidates: Vec<Candidate>, round_no: u64) {
        {
            let mut g = self.inner.lock();
            if g.in_flight {
                g.stats.skipped_in_flight += 1;
                return;
            }
            g.in_flight = true;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = InFlightGuard(Arc::clone(&this));
            this.decide_round(candidates, round_no).await;
        });
    }

    /// 执行一轮决策（含冷却与候选数检查）。直接 await 形态供
    /// [`Self::spawn_round`] 与测试使用。
    pub async fn decide_round(&self, candidates: Vec<Candidate>, round_no: u64) {
        if candidates.len() < 2 {
            let mut g = self.inner.lock();
            g.stats.skipped_no_candidates += 1;
            return;
        }
        {
            let mut g = self.inner.lock();
            if round_no < g.cooldown_until_round {
                g.stats.skipped_cooldown += 1;
                return;
            }
            g.stats.attempts += 1;
        }
        let payload = build_decide_request(&candidates, &self.endpoint.model);
        let outcome = match self.ask(&payload).await {
            Ok(resp) => classify(&resp, &candidates),
            Err(AskError::Transport(reason)) => Outcome::Transport(reason),
            Err(AskError::InvalidBody) => Outcome::Invalid("invalid_response".into()),
        };
        self.record(outcome, round_no);
    }

    /// 把有效决策应用到 `desired` 队首，返回是否真的移动了。
    ///
    /// pinned 分支不会调到这里（pin 优先级最高，见 `reselect`）；
    /// 任何不满足（无 head / 过期 / tag 不在候选里 / 已在队首）都返回
    /// `false` 并保持原分数顺序——这就是回退语义。
    pub fn apply_head(&self, desired: &mut Vec<String>, round_no: u64) -> bool {
        let g = self.inner.lock();
        apply_head_to(desired, g.head.as_ref(), round_no, self.ttl_rounds)
    }

    /// 当前有效决策的快照（面板/status/测试用）。
    pub fn head(&self) -> Option<JevHead> {
        self.inner.lock().head.clone()
    }

    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// 送审候选数（已夹到协议上限 2..=6）：调度侧用它截断分数前 N。
    pub fn candidate_count(&self) -> usize {
        self.candidate_count
    }

    /// 面板用的完整状态快照。
    pub fn status(&self) -> JevStatus {
        let g = self.inner.lock();
        JevStatus {
            provider: self.provider.to_string(),
            head: g.head.clone(),
            stats: g.stats.clone(),
        }
    }

    /// `silverq status` 行内用的紧凑摘要。
    pub fn summary(&self) -> String {
        let g = self.inner.lock();
        let head = match &g.head {
            Some(h) => format!("{}(p={:.2},r={})", h.tag, h.probability, h.decided_at_round),
            None => "none".into(),
        };
        let s = &g.stats;
        format!(
            "head={head} attempts={} ok={} fail={} err={}",
            s.attempts,
            s.successes,
            s.fallback_total(),
            s.last_error.as_deref().unwrap_or("-"),
        )
    }

    async fn ask(&self, payload: &Value) -> Result<Value, AskError> {
        let mut req = self
            .http
            .post(&self.endpoint.url)
            .bearer_auth(&self.endpoint.bearer)
            .json(payload);
        if let Some(t) = &self.endpoint.referer {
            req = req.header("HTTP-Referer", t);
        }
        if let Some(t) = &self.endpoint.title {
            req = req.header("X-Title", t);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Err(AskError::Transport("timeout".into())),
            Err(_) => return Err(AskError::Transport("network".into())),
        };
        let status = resp.status();
        if !status.is_success() {
            // 状态码是端点健康问题（401/429/5xx…），按传输失败计，
            // 固定短语不回显响应体（可能带上游信息）。
            return Err(AskError::Transport(format!("http_{}", status.as_u16())));
        }
        let text = resp
            .text()
            .await
            .map_err(|_| AskError::Transport("body_read".into()))?;
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.get("answers").is_some_and(Value::is_object) => Ok(v),
            _ => Err(AskError::InvalidBody),
        }
    }

    /// 落账 + 更新 head + 打日志。失败一律**作废已有决策**（立即回纯分数选择），
    /// 连续失败达阈值进冷却。
    fn record(&self, outcome: Outcome, round_no: u64) {
        let mut cooldown_to: Option<u64> = None;
        match outcome {
            Outcome::Selected {
                tag,
                probability,
                confidence,
            } if probability >= self.min_probability => {
                {
                    let mut g = self.inner.lock();
                    g.head = Some(JevHead {
                        tag: tag.clone(),
                        probability,
                        decided_at_round: round_no,
                    });
                    g.stats.successes += 1;
                    g.stats.consecutive_failures = 0;
                    g.stats.last_error = None;
                    g.stats.last_success_ts = crate::scheduler::persist::now_secs();
                }
                tracing::info!(tag, probability, confidence = ?confidence, round_no, "jev 决策采纳：胜出节点置顶");
            }
            other => {
                let (kind, reason) = match other {
                    Outcome::Selected { probability, .. } => (
                        Fallback::LowProbability,
                        format!("low_probability:{probability:.2}"),
                    ),
                    Outcome::Escaped(hatch) => (Fallback::Escaped, format!("escaped:{hatch}")),
                    Outcome::Invalid(r) => (Fallback::Invalid, r),
                    Outcome::Transport(r) => (Fallback::Transport, r),
                };
                {
                    let mut g = self.inner.lock();
                    g.stats.bump(kind);
                    g.stats.consecutive_failures = g.stats.consecutive_failures.saturating_add(1);
                    g.stats.last_error = Some(reason.clone());
                    g.head = None;
                    if g.stats.consecutive_failures >= self.fail_threshold {
                        g.cooldown_until_round = round_no.saturating_add(self.cooldown_rounds);
                        cooldown_to = Some(g.cooldown_until_round);
                    }
                }
                tracing::warn!(reason = %reason, round_no, "jev 决策未采纳，回退分数选择");
                if let Some(until) = cooldown_to {
                    tracing::warn!(until, "jev 连续失败达阈值，进入冷却");
                }
            }
        }
    }
}

/// `JevDecider::status()` 的序列化载体（web `/api/status` 的 `jev` 字段）。
#[derive(Debug, Clone, Serialize)]
pub struct JevStatus {
    pub provider: String,
    pub head: Option<JevHead>,
    #[serde(flatten)]
    pub stats: JevStats,
}

/// 纯函数版的队首应用：head 未过期且 tag 仍在候选里才置顶。
/// 单独导出是为了让「过期/失配/已置顶」三种分支可被直接测试。
pub fn apply_head_to(
    desired: &mut Vec<String>,
    head: Option<&JevHead>,
    round_no: u64,
    ttl_rounds: u64,
) -> bool {
    let Some(h) = head else {
        return false;
    };
    // 决策跨过的轮末数超限 → 过期（典型场景：长时间 pinned 期间不刷新，
    // 解除后不该让陈旧判断继续带队首）。
    if round_no.saturating_sub(h.decided_at_round) > ttl_rounds {
        return false;
    }
    let Some(pos) = desired.iter().position(|t| *t == h.tag) else {
        return false;
    };
    if pos == 0 {
        return false;
    }
    let tag = desired.remove(pos);
    desired.insert(0, tag);
    true
}

/// 解析单个 choice 回答 → 决策结果（选中/逃生舱/非法）。
fn classify(resp: &Value, candidates: &[Candidate]) -> Outcome {
    let ids: Vec<String> = (0..candidates.len()).map(|i| format!("node{i}")).collect();
    let mut expected: Vec<&str> = ids.iter().map(String::as_str).collect();
    expected.extend(ESCAPE_HATCHES.iter().map(|(n, _)| *n));

    let answer = resp.pointer("/answers/recommendation");
    let Some(ch) = validate_choice_answer(answer, &expected) else {
        return Outcome::Invalid("invalid_response".into());
    };
    if let Some(i) = ids.iter().position(|id| *id == ch.choice) {
        let probability = ch.probabilities.get(&ch.choice).copied().unwrap_or(0.0);
        return Outcome::Selected {
            tag: candidates[i].tag.clone(),
            probability,
            confidence: ch.confidence,
        };
    }
    // validate 保证 choice ∈ expected；到这里只可能是逃生舱之一。
    Outcome::Escaped(ch.choice)
}
