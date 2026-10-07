//! 直连拨号辅助：单一预算盖住 DNS + TCP 两段。

use std::time::Duration;
use tokio::net::TcpStream;
/// 直连 TCP 目标（带超时）。None = 失败/超时（已记日志）。
pub(super) async fn dial_direct(
    addr: impl tokio::net::ToSocketAddrs,
    timeout: Duration,
    label: String,
) -> Option<TcpStream> {
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) => {
            tracing::warn!(target = %label, "直连失败: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(target = %label, "直连超时");
            None
        }
    }
}

/// 单预算直连的结果：区分「预算内 DNS 失败」（域名没解析出来）与
/// 「预算耗尽」（DNS 或 dial 段超时）——两者回退语义不同：
/// DNS 失败不压缩候选链首响预算（解析问题，代理侧正常探测即可），
/// 拨号失败才压缩（链路本身在死，别再白等）。
pub(super) enum DirectAttempt {
    /// 直连成功（流已建立）
    Ok(TcpStream),
    /// 预算内 DNS 解析失败
    DnsFail,
    /// DNS 或 dial 段耗尽单一预算
    BudgetExhausted,
}

/// 直连（DNS 解析 + TCP dial）单一预算：`budget` 同时约束两段，
/// 不叠加（早先 DNS 超时 + dial 超时各一份，被墙域名直连侧最坏
/// 4× 预算才轮到代理兜底，浏览器早超时）。
pub(super) async fn single_budget_direct(
    host: &str,
    port: u16,
    budget: Duration,
    label: &str,
) -> DirectAttempt {
    let fused = async {
        let ip = crate::proxy::dns::resolve_host(host).await;
        let Some(ip) = ip else {
            tracing::info!(target = %label, "直连 DNS 解析失败（预算内）");
            return DirectAttempt::DnsFail;
        };
        match dial_direct((ip, port), budget, label.to_string()).await {
            Some(s) => DirectAttempt::Ok(s),
            None => DirectAttempt::BudgetExhausted,
        }
    };
    match tokio::time::timeout(budget, fused).await {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(target = %label, "直连单预算耗尽（DNS+dial）");
            DirectAttempt::BudgetExhausted
        }
    }
}
