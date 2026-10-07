//! 目标策略判定：回环判定、路线缓存门禁与候选挑选。

use std::collections::HashMap;
use std::sync::Arc;
/// 回环/私网目标判定：IP 字面量（loopback / RFC1918 私网 / 链路本地 /
/// 未指定 / v6 唯一本地）或 `localhost`（含 `*.localhost`，RFC 6761）。
/// 这类地址只在 silverq 本机或本机局域网内可达，送进代理候选链会被拨到
/// 节点自己的 loopback/LAN——轻则死候选烧满超时后才直连兜底（实测本机
/// HTTP 服务 12s+，浏览器早已超时断流），重则拿到节点侧的错误内容。
/// 本地/回环/私网目标判定：这类目标永远直连，不进代理候选链。
/// （pub：集成测试要断言"userinfo 不能骗过回环判定"；TUN 的 DIRECT 兜底
/// 反查真身后也走同一道信任边界，防 DNS 污染把私网地址喂进数据面。）
pub fn is_local_target(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
        }
        Err(_) => false,
    }
}

/// 域名级路线决策（直连 / 竞速 / 代理）是否适用于该目标。
///
/// IP 字面量目标没有"域名 → 路线"这一层：DNS 无可解析，路线缓存里的
/// Direct/Race 记录对它是无意义的。更实际的是，客户端发来的公网 IPv6
/// 目标在本机只有私网 v6 时盲拨必然 `Network is unreachable`，白白烧掉
/// 一次直连尝试（竞速场景还会拖慢整条路径）。这类目标一律交给候选链。
pub fn route_cache_eligible(host: &str, pinned: bool, china_hit: bool) -> bool {
    !pinned && !is_local_target(host) && !china_hit && host.parse::<std::net::IpAddr>().is_err()
}

/// 按 selection 顺序取前 `max_attempts` 个候选 adapter，返回 (tag, adapter) 对。
/// tag 是实际流量成败的 HP 归因落点；缺失 tag（配置漂移/节点被摘）跳过。
///
/// 抽成纯函数以便单测截断逻辑——e2e 层面这个行为被 EWMA 排序的时序淹没，
/// 测不稳（试过三版 e2e 都被"首轮测速改排序"击穿）。
pub fn pick_candidates(
    order: &[String],
    registry: &HashMap<String, Arc<dyn meow_common::adapter::ProxyAdapter>>,
    max_attempts: usize,
) -> Vec<(String, Arc<dyn meow_common::adapter::ProxyAdapter>)> {
    order
        .iter()
        .filter_map(|tag| registry.get(tag).map(|a| (tag.clone(), a.clone())))
        .take(max_attempts.max(1))
        .collect()
}
