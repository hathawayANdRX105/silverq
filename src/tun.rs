//! TUN 透明代理数据面 —— 基于 meow-listener 的 listener-tun 实现。
//!
//! 功能：创建 TUN 设备 → 接管 fake-IP 范围路由 → 所有 fake-IP 流量进 TUN
//! → 经 meow Tunnel 引擎按规则分发 → 复用 silverq 的 ProxyAdapter 做出站。

#![cfg(all(feature = "meow", feature = "meow-listener"))]

use crate::meow::Registry;
use crate::node::Node;
use crate::settings::Effective;
use meow_common::{
    AdapterType, DelayHistory, DnsMode, Metadata, Proxy, ProxyAdapter, ProxyConn, ProxyHealth,
    ProxyPacketConn, TunnelMode,
};
use meow_dns::resolver::Resolver;
use meow_listener::tun::{TunListener, TunListenerConfig, TunReady, TunRouteScope};
use meow_rules::final_rule::FinalRule;
use meow_tunnel::Tunnel;
use parking_lot::RwLock;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock as TokioRwLock;
use tracing::info;

/// 将 ProxyAdapter 包装为实现 Proxy trait（增加 alive/alive_for_url 等方法）
struct ProxyWrapper {
    inner: Arc<dyn ProxyAdapter>,
}

impl ProxyWrapper {
    fn new(inner: Arc<dyn ProxyAdapter>) -> Self {
        Self { inner }
    }
}

impl ProxyAdapter for ProxyWrapper {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn adapter_type(&self) -> AdapterType {
        self.inner.adapter_type()
    }
    fn addr(&self) -> &str {
        self.inner.addr()
    }
    fn support_udp(&self) -> bool {
        self.inner.support_udp()
    }
    async fn dial_tcp(&self, metadata: &Metadata) -> meow_common::Result<Box<dyn ProxyConn>> {
        self.inner.dial_tcp(metadata).await
    }
    async fn dial_udp(&self, metadata: &Metadata) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
        self.inner.dial_udp(metadata).await
    }
    fn health(&self) -> &ProxyHealth {
        self.inner.health()
    }
}

impl Proxy for ProxyWrapper {
    fn alive(&self) -> bool {
        true
    }
    fn alive_for_url(&self, _url: &str) -> bool {
        true
    }
    fn last_delay(&self) -> u16 {
        0
    }
    fn last_delay_for_url(&self, _url: &str) -> u16 {
        0
    }
    fn delay_history(&self) -> Vec<DelayHistory> {
        Vec::new()
    }
}

/// TUN 运行时配置（从 Effective 派生）
#[derive(Clone, Debug)]
pub struct TunConfig {
    /// TUN 设备名（如 "utun0"、"tun0"、"wintun"），None = 自动
    pub device: Option<String>,
    /// MTU，None = 自动（通常 1500/9000）
    pub mtu: Option<u16>,
    /// 是否启用 auto-route
    pub auto_route: bool,
    /// fake-ip CIDR（默认 198.18.0.0/15，兼容 mihomo/clash 风格）
    pub fake_ip_cidr: Option<String>,
    /// 排除的 CIDR（不走 TUN，如本地网段、代理服务器 IP）
    pub exclude_cidrs: Vec<String>,
    /// DNS 劫持端口（配合 fake-ip，默认 1053）
    pub dns_port: u16,
}

impl TunConfig {
    pub fn from_effective(eff: &Effective) -> Self {
        Self {
            device: eff.tun_device.clone(),
            mtu: eff.tun_mtu,
            auto_route: eff.tun_auto_route,
            fake_ip_cidr: eff.tun_fake_ip_cidr.clone(),
            exclude_cidrs: eff.tun_exclude_cidrs.clone(),
            dns_port: eff.tun_dns_port,
        }
    }
}

/// 共享选择状态：调度循环写，TUN 读
pub type SharedSelection = Arc<TokioRwLock<Vec<String>>>;

/// TUN 运行时状态（用于热更新 proxies）
struct TunRuntime {
    tunnel: Tunnel,
    registry: Registry,
    selection: SharedSelection,
    /// 当前已注册到 Tunnel 的 "silverq-auto" proxy 对应的 tag
    current_auto_tag: RwLock<Option<String>>,
}

impl TunRuntime {
    fn new(
        registry: Registry,
        selection: SharedSelection,
        _dns_port: u16,
        fake_ip_cidr: Option<String>,
    ) -> Self {
        // 创建 DNS resolver（用于 fake-IP 模式）
        let fake_ip_range = fake_ip_cidr
            .as_deref()
            .and_then(|s| ipnet::Ipv4Net::from_str(s).ok())
            .unwrap_or_else(|| ipnet::Ipv4Net::new(Ipv4Addr::new(198, 18, 0, 0), 15).unwrap());

        let resolver = Arc::new(Resolver::new(
            vec![], // upstream DNS（留空，后续可通过配置添加）
            vec![], // hosts
            DnsMode::FakeIp,
            meow_trie::DomainTrie::new(),
            false, // use_hosts
        ));

        let tunnel = Tunnel::new(resolver);
        tunnel.set_mode(TunnelMode::Rule);
        tunnel.spawn_background_tasks();

        Self {
            tunnel,
            registry,
            selection,
            current_auto_tag: RwLock::new(None),
        }
    }

    /// 同步 "silverq-auto" proxy 到 Tunnel：从 registry 读取当前 selection 的第一个 adapter，更新 Tunnel 的 proxies map
    async fn sync_proxies(&self) {
        let selection = self.selection.read().await;
        let registry = self.registry.read();

        let new_tag = selection.first().cloned();
        let mut current = self.current_auto_tag.write();

        // 只有当 selection 的第一个节点变化时才更新
        if *current != new_tag {
            let mut proxies = HashMap::new();

            if let Some(tag) = &new_tag {
                if let Some(adapter) = registry.get(tag) {
                    let wrapped =
                        Arc::new(ProxyWrapper::new(Arc::clone(adapter))) as Arc<dyn Proxy>;
                    proxies.insert(SmolStr::new("silverq-auto"), wrapped);
                    *current = Some(tag.clone());
                    info!(tag = %tag, "TUN silverq-auto proxy updated");
                }
            } else {
                // 没有可用节点，清空 silverq-auto
                *current = None;
                info!("TUN silverq-auto proxy cleared (no available nodes)");
            }

            drop(current);
            if !proxies.is_empty() {
                self.tunnel.update_proxies(proxies);
            }
        }
    }

    /// 初始化规则（首次启动时调用）
    fn init_rules(&self) {
        // 简单规则：所有流量走 silverq-auto
        // 注意：不使用 GEOIP，避免依赖 GeoIP 数据库
        let rules: Vec<Box<dyn meow_common::Rule>> = vec![Box::new(FinalRule::new("silverq-auto"))];
        self.tunnel.update_rules(rules);
    }
}

/// 启动 TUN 数据面
pub async fn run(
    config: TunConfig,
    registry: Registry,
    selection: SharedSelection,
    _nodes: Vec<Node>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!(
        device = config.device.as_deref().unwrap_or("auto"),
        auto_route = config.auto_route,
        "启动 TUN 透明代理"
    );

    // 解析 fake-ip CIDR
    let fake_ip_cidr = config
        .fake_ip_cidr
        .clone()
        .unwrap_or_else(|| "198.18.0.0/15".to_string());
    let fake_ip_net = ipnet::Ipv4Net::from_str(&fake_ip_cidr)
        .map_err(|e| format!("invalid fake_ip_cidr: {e}"))?;

    // 创建 TUN 运行时
    let runtime = Arc::new(TunRuntime::new(
        registry.clone(),
        selection.clone(),
        config.dns_port,
        Some(fake_ip_cidr.clone()),
    ));

    // 初始化规则
    runtime.init_rules();

    // 启动 proxies 同步任务（每 5 秒同步一次 selection 变化）
    let sync_runtime = Arc::clone(&runtime);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            sync_runtime.sync_proxies().await;
        }
    });

    // 配置 TUN Listener
    let listener_config = TunListenerConfig {
        device: config.device,
        mtu: config.mtu.unwrap_or(1500),
        inet4_address: fake_ip_net, // TUN 设备分配的 IP（在 fake-IP 范围内）
        auto_route: config.auto_route,
        route_scope: TunRouteScope::FakeIp, // 默认 fake-IP 模式，跨平台无环路
        outbound_interface: None,           // fake-IP 模式不需要
        dns_hijack: true,                   // 劫持 UDP :53 到内置 DNS
        udp_timeout: Duration::from_secs(60),
        max_connections: 256,
    };

    // 创建并运行 TUN Listener
    let listener = TunListener::new(
        runtime.tunnel.clone(),
        listener_config,
        "silverq-tun".to_string(),
    );

    // 设置 readiness channel
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<TunReady>();
    let listener = listener.with_readiness_signal(ready_tx);

    // 启动 listener（阻塞直到出错/取消）
    let run_result = listener.run().await;

    // 等待 readiness 信号（如果 run 很快返回，也检查 readiness）
    let _ = ready_rx.await;

    run_result
}
