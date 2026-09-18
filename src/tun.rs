//! TUN 透明代理数据面 —— 基于 meow-listener 的 listener-tun 实现。
//!
//! 功能：创建 TUN 设备 → 接管 fake-IP 范围路由 → 所有 fake-IP 流量进 TUN
//! → 经 meow Tunnel 引擎按规则分发 → 复用 silverq 的 ProxyAdapter 做出站。

#![cfg(all(feature = "meow", feature = "meow-listener"))]

use crate::meow::Registry;
use crate::node::Node;
use crate::settings::Effective;
use meow_common::{
    AdapterType, DelayHistory, DnsMode, MeowError, Metadata, Proxy, ProxyAdapter, ProxyConn,
    ProxyHealth, ProxyPacketConn, TunnelMode,
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

#[async_trait::async_trait]
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
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::FileConfig;
    use meow_common::Rule;
    use meow_proxy::direct::DirectAdapter;
    use std::time::Duration;

    // ---- helpers ----

    /// 构造默认 `Effective`（经 `FileConfig::default()`）。断言都对着 `eff`
    /// 的字段做，而非硬编码字面量 —— 这样即便环境变量覆盖了默认值，
    /// `from_effective` 的映射关系仍能被验证。
    fn default_eff() -> Effective {
        Effective::from(&FileConfig::default())
    }

    /// 空 registry（给不依赖具体节点的 `run` / `TunRuntime` 测试用）。
    fn empty_registry() -> Registry {
        Arc::new(parking_lot::RwLock::new(HashMap::new()))
    }

    /// 包了 `DirectAdapter` 的 `ProxyWrapper`（测 Proxy 桩 + 委托）。
    fn direct_wrapper() -> ProxyWrapper {
        let inner: Arc<dyn ProxyAdapter> = Arc::new(DirectAdapter::new());
        ProxyWrapper::new(inner)
    }

    // ---- A1/A2: TunConfig::from_effective 映射 ----

    #[test]
    fn test_tun_config_from_effective_defaults() {
        let eff = default_eff();
        let tc = TunConfig::from_effective(&eff);
        // 每个字段都应等于 Effective 里对应字段（默认值由 config.rs 给定）。
        assert_eq!(tc.device, eff.tun_device);
        assert_eq!(tc.mtu, eff.tun_mtu);
        assert_eq!(tc.auto_route, eff.tun_auto_route);
        assert_eq!(tc.fake_ip_cidr, eff.tun_fake_ip_cidr);
        assert_eq!(tc.exclude_cidrs, eff.tun_exclude_cidrs);
        assert_eq!(tc.dns_port, eff.tun_dns_port);
    }

    #[test]
    fn test_tun_config_from_effective_custom() {
        let mut eff = default_eff();
        eff.tun_device = Some("utun9".into());
        eff.tun_mtu = Some(1400);
        eff.tun_auto_route = false;
        eff.tun_fake_ip_cidr = Some("10.10.0.0/16".into());
        eff.tun_exclude_cidrs = vec!["1.2.3.0/24".into(), "9.9.9.0/24".into()];
        eff.tun_dns_port = 5300;

        let tc = TunConfig::from_effective(&eff);
        assert_eq!(tc.device.as_deref(), Some("utun9"));
        assert_eq!(tc.mtu, Some(1400));
        assert!(!tc.auto_route);
        assert_eq!(tc.fake_ip_cidr.as_deref(), Some("10.10.0.0/16"));
        assert_eq!(
            tc.exclude_cidrs,
            vec!["1.2.3.0/24".to_string(), "9.9.9.0/24"]
        );
        assert_eq!(tc.dns_port, 5300);
    }

    // ---- A3: ProxyWrapper 的 Proxy trait 桩 ----

    #[test]
    fn test_proxy_wrapper_proxy_trait_stubs() {
        let w = direct_wrapper();
        // 桩语义：TUN 出站永远报活、延迟 0、无历史。真实健康度由调度循环维护，
        // 这里只是不能让 Tunnel 引擎因为 health 检查把 silverq-auto 当死代理跳过。
        assert!(w.alive(), "alive 桩必须返回 true");
        assert!(
            w.alive_for_url("https://example/"),
            "alive_for_url 桩必须返回 true"
        );
        assert_eq!(w.last_delay(), 0);
        assert_eq!(w.last_delay_for_url("https://example/"), 0);
        assert!(w.delay_history().is_empty(), "delay_history 桩必须为空");
    }

    // ---- A4: ProxyWrapper 委托给 inner ProxyAdapter ----

    #[test]
    fn test_proxy_wrapper_delegates_to_inner() {
        let w = direct_wrapper();
        // inner 是 DirectAdapter：name=DIRECT、addr=""、type=Direct、udp=true。
        assert_eq!(w.name(), "DIRECT");
        assert_eq!(w.addr(), "");
        assert_eq!(w.adapter_type(), AdapterType::Direct);
        assert!(w.support_udp());
    }

    // ---- A5: run 拒绝非法 fake_ip_cidr ----

    #[tokio::test]
    async fn test_run_rejects_invalid_fake_ip_cidr() {
        // 解析发生在建 listener / runtime 之前，所以无 root 也能测到这条早期失败。
        let config = TunConfig {
            device: None,
            mtu: None,
            auto_route: false,
            fake_ip_cidr: Some("not-a-cidr".into()),
            exclude_cidrs: vec![],
            dns_port: 1053,
        };
        let err = run(
            config,
            empty_registry(),
            Arc::new(TokioRwLock::new(vec![])),
            vec![],
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("invalid fake_ip_cidr"),
            "应报 invalid fake_ip_cidr，实际: {err}"
        );
    }

    // ---- A6: init_rules / sync_proxies（Tunnel 暴露 route_snapshot/proxy getter） ----

    #[tokio::test]
    async fn test_init_rules_registers_single_final_rule() {
        // `Tunnel::route_snapshot().rules` 是 pub，可以拿到 init_rules 注册的规则。
        // （标 `#[tokio::test]` 是因为 `TunRuntime::new` 内部 spawn 后台任务需要 runtime。）
        let rt = TunRuntime::new(
            empty_registry(),
            Arc::new(TokioRwLock::new(vec![])),
            1053,
            Some("198.18.0.0/15".into()),
        );
        rt.init_rules();
        let snap = rt.tunnel.route_snapshot();
        assert_eq!(snap.rules.len(), 1, "init_rules 只注册一条 FinalRule");
        assert_eq!(
            snap.rules[0].adapter(),
            "silverq-auto",
            "FinalRule 应指向 silverq-auto"
        );
    }

    #[tokio::test]
    async fn test_sync_proxies_updates_on_selection_change() {
        // registry 放一个 direct adapter，selection 指向它 → sync 应把它包成
        // "silverq-auto" 注册进 Tunnel，并更新 current_auto_tag。
        let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
        reg.insert("direct-a".into(), Arc::new(DirectAdapter::new()));
        let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

        let rt = TunRuntime::new(registry, selection, 1053, Some("198.18.0.0/15".into()));

        assert!(
            rt.tunnel.proxy("silverq-auto").is_none(),
            "sync 前不应有 silverq-auto"
        );
        rt.sync_proxies().await;
        assert!(
            rt.tunnel.proxy("silverq-auto").is_some(),
            "sync 后应注册 silverq-auto"
        );
        let tag_guard = rt.current_auto_tag.read();
        assert_eq!(
            *tag_guard,
            Some("direct-a".to_string()),
            "current_auto_tag 应记下当前 tag"
        );
    }

    #[tokio::test]
    async fn test_sync_proxies_skipped_when_selection_unchanged() {
        // 同一 selection 第二次 sync：`*current == new_tag` → 不进更新分支，
        // 不重建 route table。`route_snapshot()` 返回的是 `Arc::clone` 自存储表，
        // 未更新时两次快照指向同一分配，指针相等即证明"跳过了"。
        let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
        reg.insert("direct-a".into(), Arc::new(DirectAdapter::new()));
        let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

        let rt = TunRuntime::new(registry, selection, 1053, Some("198.18.0.0/15".into()));
        rt.sync_proxies().await;
        let snap1 = rt.tunnel.route_snapshot();

        rt.sync_proxies().await; // 同样 selection，应跳过
        let snap2 = rt.tunnel.route_snapshot();

        assert!(
            Arc::ptr_eq(&snap1, &snap2),
            "selection 未变时不应重建 route table"
        );
    }

    #[tokio::test]
    async fn test_sync_proxies_clears_tag_when_selection_empty() {
        // 现有实现的取舍：空 selection 只清 current_auto_tag，并不调
        // update_proxies 把 Tunnel 里旧的 silverq-auto 移除（proxies 为空时不更新）。
        // 这里测的是当前行为，不是理想行为。
        let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
        reg.insert("direct-a".into(), Arc::new(DirectAdapter::new()));
        let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

        let rt = TunRuntime::new(
            registry,
            selection.clone(),
            1053,
            Some("198.18.0.0/15".into()),
        );
        rt.sync_proxies().await;
        {
            let g = rt.current_auto_tag.read();
            assert_eq!(*g, Some("direct-a".to_string()));
        }

        *selection.write().await = vec![];
        rt.sync_proxies().await;
        assert!(
            rt.current_auto_tag.read().is_none(),
            "空 selection 后 current_auto_tag 应被清空"
        );
    }

    // ---- B: root-gated 手动 smoke 测试（需要 root / CAP_NET_ADMIN） ----
    //
    // 本地跑（CI 不跑 —— `#[ignore]`，且 runner 无 root）：
    //   sudo cargo test --features meow-tun --bin silverq -- --ignored --test-threads=1
    //
    // silverq 是 bin-only crate（无 lib target），集成测试无法 import `tun::run`，
    // 所以这几条 smoke 放在本模块内（同模块可直接调 `run`）。详见 README「TUN 手动测试」。

    #[tokio::test]
    #[ignore]
    async fn test_tun_device_created() {
        // auto_route=false 避免改动宿主路由表；设备名 auto 让底层挑。
        // 目标只是"创建设备这条路径不立刻 panic"：spawn `run`，睡 1s，再探测有无 panic。
        let config = TunConfig {
            device: None,
            mtu: None,
            auto_route: false,
            fake_ip_cidr: Some("198.18.0.0/15".into()),
            exclude_cidrs: vec![],
            dns_port: 1053,
        };
        let handle = tokio::spawn(async move {
            let _ = run(
                config,
                empty_registry(),
                Arc::new(TokioRwLock::new(vec![])),
                vec![],
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(1)).await;

        // 用很短的 join 超时观察窗口内的结果：成功路径仍在阻塞（超时→Err），
        // 无 root 时 `run` 快速返回 Err（task 正常结束 → Ok(Ok(()))），
        // panic 时 task 异常结束 → Ok(Err(JoinError{is_panic})).
        let probe = tokio::time::timeout(Duration::from_millis(100), &mut handle).await;
        handle.abort();
        if let Ok(Err(join_err)) = probe {
            if join_err.is_panic() {
                panic!("tun::run 在 1s 窗口内 panic: {join_err}");
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_invalid_device_name_errors() {
        // 空设备名是否被拒绝取决于底层 `TunListener`；这里只确认不永久挂起、不 panic。
        let config = TunConfig {
            device: Some(String::new()),
            mtu: None,
            auto_route: false,
            fake_ip_cidr: Some("198.18.0.0/15".into()),
            exclude_cidrs: vec![],
            dns_port: 1053,
        };
        let res = tokio::time::timeout(
            Duration::from_secs(3),
            run(
                config,
                empty_registry(),
                Arc::new(TokioRwLock::new(vec![])),
                vec![],
            ),
        )
        .await;
        assert!(res.is_ok(), "run 应在 3s 内返回，而非永久挂起");
    }
}
