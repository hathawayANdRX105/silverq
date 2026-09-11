//! 极简 Web 面板：内嵌单页 HTML + 3 个 JSON 端点。
//!
//! # 为什么手写 HTTP 而不用 axum/tiny_http
//!
//! 面板只需要 3 个端点 + 一个静态 HTML。引 axum/tiny_http 会拖几十个传递依赖；
//! 手写请求行解析 + JSON 拼接 ~150 行，零新依赖。请求格式被严格限定
//! （`GET /api/...`），解析器只服务这个场景，不是通用 HTTP 实现。
//!
//! # 安全边界
//!
//! 默认绑 `127.0.0.1`（`SILVERQ_WEB_LISTEN` 可改）。**没有认证**——
//! `select` 端点能改节点，别绑到 0.0.0.0。
#![cfg(feature = "meow")]

use crate::ctl::CtlState;
use serde::Serialize;
use serde_json::Value;
use std::str::FromStr;
use tokio::io::AsyncWriteExt as _;

/// clash_api 的 WebSocket 端点。metacubexd 启动时先连这些 WS，
/// 连不上就认为后端不可用，**所有 REST 数据拉取都不会发生**（实测踩过）。
const WS_PATHS: &[(&str, WsKind)] = &[
    ("/traffic", WsKind::Traffic),
    ("/memory", WsKind::Memory),
    ("/connections", WsKind::Connections),
    ("/logs", WsKind::Logs),
];

#[derive(Clone, Copy)]
enum WsKind {
    Traffic,
    Memory,
    Connections,
    Logs,
}

impl WsKind {
    fn payload(self) -> &'static str {
        match self {
            Self::Traffic => "{\"up\":0,\"down\":0}",
            Self::Memory => "{\"inuse\":0,\"oslimit\":0}",
            Self::Connections => {
                "{\"downloadTotal\":0,\"uploadTotal\":0,\"connections\":[],\"memory\":0}"
            }
            Self::Logs => "",
        }
    }
}

/// WebSocket 握手 + 周期推 JSON。浏览器发的帧（ping/close）最小处理。
async fn ws_serve(mut socket: TcpStream, kind: WsKind, req_text: &str) {
    let Some(key) = req_text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
    else {
        return;
    };
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    use base64::Engine as _;
    let accept = base64::engine::general_purpose::STANDARD.encode(h.finalize());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if socket.write_all(resp.as_bytes()).await.is_err() {
        return;
    }

    let (mut rd, mut wr) = socket.split();
    // 写循环：每秒推一帧 payload（logs 只保活不推数据）
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let p = kind.payload();
                if p.is_empty() { continue; }
                if write_frame(&mut wr, 0x81, p.as_bytes()).await.is_err() {
                    return;
                }
            }
            n = rd.read(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => return,           // 对端关闭
                    Ok(_) => {}                          // ping/pong/忽略
                }
            }
        }
    }
}

/// 服务端→客户端帧：FIN+opcode，len<126 直接编码（我们的 payload 都很小）。
async fn write_frame(
    wr: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let head = [opcode, payload.len() as u8];
    wr.write_all(&head).await?;
    wr.write_all(payload).await?;
    wr.flush().await
}
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

/// 面板 HTML：原生 JS 轮询 /api/status，3s 刷新。
/// include_str! 内嵌进二进制，无需静态文件服务。
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[derive(Debug, Serialize)]
struct NodeJson<'a> {
    tag: &'a str,
    /// 实测延迟 EWMA（毫秒）；null = 从未测通。
    /// **纯延迟，不含失败罚分** —— 罚分单独在 `failures` 里。
    ewma: Option<u64>,
    /// 连续失败次数。>0 表示这个节点当前在被降权观察。
    failures: u32,
    samples: u32,
    /// 是否测过（无论成败）。用来把「测了但全失败」和「还没轮到」分开：
    /// samples 只在成功时累加，死节点池里 samples==0 的绝大多数其实测过了。
    probed: bool,
    /// 距上次探测的秒数；u64::MAX = 从未探测。面板用它显示数据新鲜度。
    last_probe_secs: u64,
    /// 延迟历史 (unix秒, 毫秒)，面板画图。只含成功测速。
    history: Vec<(u64, u64)>,
    /// 是否在当前 selection 里
    active: bool,
    /// 是否是当前首选
    primary: bool,
}

#[derive(Debug, Serialize)]
struct StatusJson<'a> {
    pinned: bool,
    nodes_path: &'a str,
    selection: &'a [String],
    nodes: Vec<NodeJson<'a>>,
}

fn json_response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// JS 响应。service worker 脚本必须以 JS MIME 返回，否则浏览器拒绝注册/更新。
fn js_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/javascript; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nService-Worker-Allowed: /ui/\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn html_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

async fn status_json(state: &CtlState) -> String {
    let pool = state.pool.read().await;
    let selection = state.selection.read().await;
    let pinned = state.pinned.load(std::sync::atomic::Ordering::Relaxed);
    let nodes_path = state.nodes_path.lock().clone();

    // 按 EWMA 升序展示（面板视角：最好的在前）
    let mut nodes: Vec<&Node> = pool.iter().collect();
    nodes.sort_by(|a, b| a.score().partial_cmp(&b.score()).unwrap());

    let nodes_json: Vec<NodeJson> = nodes
        .iter()
        .map(|n| NodeJson {
            tag: &n.tag,
            ewma: if n.ewma.is_finite() {
                Some(n.ewma as u64)
            } else {
                None
            },
            samples: n.samples,
            failures: n.consecutive_failures,
            probed: n.last_measured.is_some(),
            last_probe_secs: n
                .last_measured
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(u64::MAX),
            history: n.history.iter().map(|(t, ms)| (*t, *ms as u64)).collect(),
            active: selection.contains(&n.tag),
            primary: selection.first().map(|s| s == &n.tag).unwrap_or(false),
        })
        .collect();

    let payload = StatusJson {
        pinned,
        nodes_path: &nodes_path,
        selection: &selection,
        nodes: nodes_json,
    };
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
}

/// 处理一条连接。返回要写的响应文本。
async fn handle_request(buf: &[u8], state: &CtlState) -> Option<String> {
    // 请求行：METHOD SP PATH SP VERSION
    let text = std::str::from_utf8(buf).ok()?;
    let mut parts = text.split_whitespace();
    let method = parts.next()?;
    let full_path = parts.next()?;
    let (path, query) = match full_path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (full_path, ""),
    };

    match (method, path) {
        ("GET", "/") => Some(html_response(DASHBOARD_HTML)),
        ("GET", "/api/status") => Some(json_response(200, &status_json(state).await)),
        ("GET", "/api/health") => Some(json_response(200, "{\"ok\":true}")),
        ("POST", "/api/select") => {
            // body: {"tag": "..."} 或 {"tag":"auto"}
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
            let tag = body
                .split("\"tag\"")
                .nth(1)
                .and_then(|s| s.split('"').nth(1))
                .map(|s| s.to_string());
            let tag = tag?;
            let reply = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(super::ctl::do_select_public(state, &tag))
            });
            Some(json_response(200, &reply))
        }
        // ---- clash 兼容 API（metacubexd 等dashboard 的对接面）----
        ("GET", "/version") => Some(json_response(
            200,
            &format!(
                "{{\"version\":\"silverq {}\",\"meta\":true,\"premium\":true}}",
                env!("CARGO_PKG_VERSION")
            ),
        )),
        ("GET", "/proxies") => Some(json_response(200, &proxies_json(state).await)),
        ("PUT", p) if p.starts_with("/proxies/") => {
            // body: {"name": "节点名"}。选中组内成员 = 钉住；"auto" = 解钉。
            let name = text
                .split("\"name\"")
                .nth(1)
                .and_then(|s| s.split('"').nth(1));
            let Some(name) = name else {
                return Some(json_response(400, "{\"message\":\"missing name\"}"));
            };
            let reply = crate::ctl::do_select_public(state, name).await;
            if reply.contains("\"ok\":true") {
                Some(NO_CONTENT.into())
            } else {
                Some(json_response(400, &reply))
            }
        }
        ("GET", p) if p.starts_with("/proxies/") && p.ends_with("/delay") => {
            // 按需单节点测延迟（面板"测一测"）。?url=&timeout=
            let tag = p.trim_start_matches("/proxies/").trim_end_matches("/delay");
            let timeout_ms = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("timeout="))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5000)
                .clamp(500, 15_000);
            delay_probe(state, tag, timeout_ms).await
        }
        ("GET", p) if p.starts_with("/proxies/") => {
            let name = p.trim_start_matches("/proxies/");
            let d = Value::from_str(&proxies_json(state).await).ok()?;
            let Some(one) = d["proxies"].get(name).cloned() else {
                return Some(json_response(404, "{\"message\":\"unknown proxy\"}"));
            };
            Some(json_response(200, &one.to_string()))
        }
        ("GET", "/configs") => Some(json_response(200, &configs_json(state))),
        ("PATCH", "/configs") => {
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
            Some(apply_config_patch(state, body))
        }
        // providers 必须是「名字 -> provider」的字典。早先误填成单个
        // provider 对象，zashboard 的 Object.values 遍历到数字/数组后
        // 在 provider.proxies 上静默抛错，整个 proxies 页数据为空（实测）。
        ("GET", "/providers/proxies") => Some(json_response(200, "{\"providers\":{}}")),
        ("GET", "/providers/rules") => Some(json_response(200, "{\"providers\":{}}")),
        ("GET", "/connections") => Some(json_response(
            200,
            "{\"downloadTotal\":0,\"uploadTotal\":0,\"connections\":[],\"memory\":0}",
        )),
        ("GET", "/rules") => Some(json_response(200, "{\"rules\":[]}")),
        // zashboard 是 PWA：workbox 预缓存 index.html，SW 一旦注册，服务端对
        // index 的注入（首访引导）就永久失效 —— 页面直接从缓存出，不再经过我们。
        // 实测：文档里 hasBootstrap=false、被钉死在 #/setup。
        // 这里把注册脚本换成「注销 + 清缓存」，让已注册的 SW 自我卸载、
        // 新访问不再注册。本地面板不需要离线能力，代价为零。
        ("GET", "/ui/registerSW.js") => Some(js_response(
            "if('serviceWorker' in navigator){navigator.serviceWorker.getRegistrations().then(function(rs){var n=rs.length;rs.forEach(function(r){r.unregister()});if(window.caches){caches.keys().then(function(ks){ks.forEach(function(k){caches.delete(k)});if(n)location.reload();})}else if(n){location.reload();}})}",
        )),
        // 已缓存旧 sw.js 的浏览器会来取更新：给一个自我注销的空 SW。
        ("GET", "/ui/sw.js") => Some(js_response(
            "self.addEventListener('install',function(){self.skipWaiting()});self.addEventListener('activate',function(e){e.waitUntil((async function(){try{for(const k of await caches.keys())await caches.delete(k)}catch(err){}await self.registration.unregister();for(const c of await self.clients.matchAll())c.navigate(c.url)})())});",
        )),
        // zashboard 静态资源。/ui/config.js 动态注入后端地址（= 本服务 origin）。
        ("GET", "/ui/config.js") => {
            let host = header(text, "Host").unwrap_or_else(|| "127.0.0.1".into());
            Some(html_response(&format!(
                "window.__METACUBEXD_CONFIG__ = {{ defaultBackendURL: 'http://{host}', githubToken: '' }};",
            )))
        }
        ("GET", p) if p == "/ui" || p == "/ui/" || p.starts_with("/ui/") => {
            serve_ui(state, p.trim_start_matches("/ui")).await
        }
        _ => {
            Some("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into())
        }
    }
}

fn header(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find(|l| {
            l.to_ascii_lowercase()
                .starts_with(&format!("{}:", name.to_ascii_lowercase()))
        })
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
}

const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\n\r\n";

/// 按需单节点测延迟：metacubexd 的"测一测"按钮。
async fn delay_probe(state: &CtlState, tag: &str, timeout_ms: u64) -> Option<String> {
    use meow_proxy::health::url_test;
    use std::time::Duration;
    let adapter = {
        let guard = state.registry.read();
        guard.get(tag).cloned()
    };
    let Some(adapter) = adapter else {
        return Some(json_response(404, "{\"message\":\"unknown proxy\"}"));
    };
    match url_test(
        adapter.as_ref(),
        &state.probe_url,
        Some("200,204"),
        Duration::from_millis(timeout_ms),
    )
    .await
    {
        Ok(delay) => Some(json_response(200, &format!("{{\"delay\":{delay}}}"))),
        Err(_) => Some(json_response(
            502,
            "{\"message\":\"An error occurred when testing the proxy\"}",
        )),
    }
}

/// GET /configs：字段够 metacubexd 配置页渲染即可，银 q 特有调参附加在下。
fn configs_json(state: &CtlState) -> String {
    let t = state.tuning.read().clone();
    let payload = serde_json::json!({
        "port": 0, "socks-port": 0,
        "redir-port": 0, "tproxy-port": 0,
        "mixed-port": 17321,
        "allow-lan": false, "bind-address": "*",
        "mode": "Rule",
        "mode-list": ["Rule", "direct", "global"],
        "log-level": "info", "ipv6": false,
        "tun": null,
        // silverq 附加字段（metacubexd 忽略未知字段；我们的工具可读）
        "silverq": {
            "capacity": t.capacity,
            "batch_size": t.batch_size,
            "interval_secs": t.interval_secs,
            "timeout_ms": t.timeout_ms,
            "concurrency": t.concurrency,
            "timeout_penalty": t.timeout_penalty,
            "fallback_attempts": t.fallback_attempts,
        },
    });
    payload.to_string()
}

/// PATCH /configs：只认 silverq 调参键（clash 的 mode/log-level 等收下不响）。
/// 热生效，不写回 TOML（改写用户带注释的配置文件是破坏性的）。
fn apply_config_patch(state: &CtlState, body: &str) -> String {
    let Ok(v) = Value::from_str(body.trim()) else {
        return json_response(400, "{\"message\":\"invalid json\"}");
    };
    let get = |k: &str| v.get(k);
    let mut t = state.tuning.read().clone();
    if let Some(n) = get("capacity").and_then(|x| x.as_u64()) {
        t.capacity = n as usize;
    }
    if let Some(n) = get("batch_size").and_then(|x| x.as_u64()) {
        t.batch_size = n as usize;
    }
    if let Some(n) = get("interval_secs").and_then(|x| x.as_u64()) {
        t.interval_secs = n;
    }
    if let Some(n) = get("timeout_ms").and_then(|x| x.as_u64()) {
        t.timeout_ms = n;
    }
    if let Some(n) = get("concurrency").and_then(|x| x.as_u64()) {
        t.concurrency = n as usize;
    }
    if let Some(n) = get("timeout_penalty").and_then(|x| x.as_f64()) {
        t.timeout_penalty = n;
    }
    if let Some(n) = get("fallback_attempts").and_then(|x| x.as_u64()) {
        t.fallback_attempts = n as usize;
    }
    *state.tuning.write() = t.validated();
    NO_CONTENT.into()
}

/// GET /proxies：silverq-active 组（Selector）+ 全体节点。
/// 银q 特有数据（failures/samples/probed/ewma 说明）作为附加字段捎带。
async fn proxies_json(state: &CtlState) -> String {
    let pool = state.pool.read().await;
    let selection = state.selection.read().await;
    let protocols = state.protocols.lock().clone();

    let history_of = |n: &crate::node::Node| -> Vec<serde_json::Value> {
        n.history
            .iter()
            .map(|(t, ms)| serde_json::json!({"time": rfc3339(*t), "delay": *ms as u64}))
            .collect()
    };

    let mut proxies = serde_json::Map::new();

    for n in pool.iter() {
        proxies.insert(
            n.tag.clone(),
            serde_json::json!({
                "type": protocols.get(&n.tag).map(|s| s.as_str()).unwrap_or("Unknown"),
                "name": n.tag,
                "udp": true,
                "history": history_of(n),
            }),
        );
    }

    // mihomo/sing-box 语义：组是"特殊节点"，与节点同层。
    // 字段保持与 sing-box clash_api 完全一致 —— metacubexd 按这个形状解析，
    // 多余字段（alive/自定义数据）曾导致整页 No data（实测）。
    let now = selection.first().cloned().unwrap_or_else(|| "auto".into());
    proxies.insert(
        "silverq-active".into(),
        serde_json::json!({
            "type": "Selector", "name": "silverq-active",
            "now": now, "all": selection.iter().cloned().collect::<Vec<_>>(),
            "history": [], "udp": true,
        }),
    );
    proxies.insert(
        "auto".into(),
        serde_json::json!({
            "type": "Fallback", "name": "auto", "now": now,
            "all": selection.iter().cloned().collect::<Vec<_>>(),
            "history": [], "udp": true,
        }),
    );
    // GLOBAL：clash 惯例的顶层组，metacubexd proxies 页依赖它
    let mut global_all: Vec<String> = vec!["auto".into()];
    global_all.extend(pool.iter().map(|n| n.tag.clone()));
    proxies.insert(
        "GLOBAL".into(),
        serde_json::json!({
            "type": "Fallback", "name": "GLOBAL", "now": now,
            "all": global_all, "history": [], "udp": true,
        }),
    );

    serde_json::to_string(&serde_json::json!({ "proxies": proxies })).unwrap_or_default()
}

/// unix 秒 → RFC3339 UTC（无 chrono 依赖，civil_from_days 算法）。
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// metacubexd 静态文件（磁盘目录，二进制不内嵌 —— 8MB 内嵌会让 RSS 永久膨胀）。
async fn serve_ui(state: &CtlState, rel: &str) -> Option<String> {
    let rel = rel.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.contains("..") {
        return Some(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        );
    }
    let path = std::path::Path::new(&state.ui_dir).join(rel);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => {
            return Some(
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            )
        }
    };
    // index.html 注入引导脚本（见 inject_bootstrap 文档）
    let bytes = if rel == "index.html" {
        inject_bootstrap(&String::from_utf8_lossy(&bytes)).into_bytes()
    } else {
        bytes
    };
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    };
    Some(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        bytes.len()
    ) + &String::from_utf8_lossy(&bytes))
}

async fn handle_one(mut socket: TcpStream, state: Arc<CtlState>) {
    use tokio::io::AsyncWriteExt;
    let mut buf = vec![0u8; 8192];
    let n = match socket.read(&mut buf).await {
        Ok(n) => n,
        Err(_) => return,
    };
    let text = String::from_utf8_lossy(&buf[..n]).to_string();

    // WebSocket 升级请求在生成普通响应前拦截
    let first_line = text.lines().next().unwrap_or("");
    if let Some((_m, path)) = first_line.split_once(' ') {
        let path = path
            .split(' ')
            .next()
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("");
        if let Some((_, kind)) = WS_PATHS.iter().find(|(p, _)| *p == path) {
            ws_serve(socket, *kind, &text).await;
            return;
        }
    }

    if let Some(resp) = handle_request(&buf[..n], &state).await {
        let _ = socket.write_all(resp.as_bytes()).await;
        let _ = socket.flush().await;
    }
}

/// 启动 Web 面板。与 inbound / ctl 并列的第三个监听任务。
pub async fn run(
    listener_addr: &str,
    state: Arc<CtlState>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(listener_addr).await?;
    tracing::info!(addr = listener_addr, "silverq web dashboard listening");

    loop {
        let (socket, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(handle_one(socket, state));
    }
}

use crate::node::Node;

/// serve index.html 时注入引导脚本：zashboard 的后端列表为空时，自动跳到
/// `?hostname=&port=` 参数地址 —— 它解析后自动落到 proxies 页，用户零输入。
/// silverq 本身无认证，setup 表单的 Password 留空即可。
///
/// 判定用 `setup/api-list`（后端列表，未配置时是 `[]`）。早先按 `config/*`
/// 前缀判断，但 zashboard 一进页面就会写 `config/proxy-folders` 等设置键，
/// 于是首访之后引导永久失效、被钉死在 #/setup（实测）。
fn inject_bootstrap(html: &str) -> String {
    let bootstrap = "<script>(function(){try{if(location.search)return;var l=localStorage.getItem('setup/api-list');if(l&&JSON.parse(l).length)return;location.replace(location.origin+'/ui/?hostname='+location.hostname+'&port='+location.port);}catch(e){}})();</script>";
    html.replace("</body>", &format!("{bootstrap}</body>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 引导脚本只注入 index.html 的 </body> 前，且无 </body> 时原样返回。
    #[test]
    fn inject_bootstrap_appends_before_body_close() {
        let html = "<html><body>x</body></html>";
        let out = inject_bootstrap(html);
        assert!(out.contains("<script>"), "必须注入 script");
        assert!(
            out.contains("setup/api-list"),
            "必须按后端列表判空，避免首访后引导失效"
        );
        assert!(out.ends_with("</body></html>"), "注入点必须在 </body> 前");
        // 无 </body> 的输入原样返回（真实 index.html 恒有 </body>）
        assert_eq!(inject_bootstrap("<html>"), "<html>");
    }

    /// 调参夹紧：面板/ PATCH 传任意值都不会把调度打坏。
    #[test]
    fn tuning_validation_clamps() {
        use crate::settings::RuntimeTuning;
        let t = RuntimeTuning {
            capacity: 9999,
            batch_size: 0,
            interval_secs: 1,
            timeout_ms: 10,
            concurrency: 0,
            timeout_penalty: -5.0,
            fallback_attempts: 100,
        }
        .validated();
        assert_eq!(t.capacity, 50);
        assert_eq!(t.batch_size, 1);
        assert_eq!(t.interval_secs, 5);
        assert_eq!(t.timeout_ms, 500);
        assert_eq!(t.concurrency, 1);
        assert_eq!(t.timeout_penalty, 100.0);
        assert_eq!(t.fallback_attempts, 10);
    }

    /// rfc3339 必须产出标准 ISO 时间，dayjs（zashboard 的解析器）才认。
    #[test]
    fn rfc3339_formats_known_epochs() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(86_400), "1970-01-02T00:00:00Z");
        // 2026-09-10T08:00:00Z == 1789027200（与 python 交叉核对）
        assert_eq!(rfc3339(1_789_027_200), "2026-09-10T08:00:00Z");
        // 闰年 2024-02-29
        assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
