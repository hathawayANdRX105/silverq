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
    let path = parts.next()?.split('?').next()?; // 忽略 query string

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
        _ => {
            Some("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into())
        }
    }
}

async fn handle_one(mut socket: TcpStream, state: Arc<CtlState>) {
    let mut buf = vec![0u8; 8192];
    let n = match socket.read(&mut buf).await {
        Ok(n) => n,
        Err(_) => return,
    };
    if let Some(resp) = handle_request(&buf[..n], &state).await {
        use tokio::io::AsyncWriteExt;
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
