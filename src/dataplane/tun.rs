//! TUN 透明代理数据面 —— 基于 meow-listener 的 listener-tun 实现。
//!
//! 功能：创建 TUN 设备 → 接管 fake-IP 范围路由 → 所有 fake-IP 流量进 TUN
//! → 经 meow Tunnel 引擎按规则分发 → 复用 silverq 的 ProxyAdapter 做出站。

#![cfg(all(feature = "meow", feature = "meow-listener"))]

use crate::config::settings::Effective;
use crate::proxy::meow::Registry;

use meow_common::{
    AdapterType, DelayHistory, DnsMode, Metadata, Proxy, ProxyAdapter, ProxyConn, ProxyHealth,
    ProxyPacketConn, TunnelMode,
};
use meow_dns::fakeip::{MemoryStore, Pool, Store};
use meow_dns::resolver::Resolver;
use meow_listener::tun::{TunListener, TunListenerConfig, TunReady, TunRouteScope};
use meow_rules::final_rule::FinalRule;
use meow_rules::ipcidr::IpCidrRule;
use meow_tunnel::Tunnel;
use parking_lot::RwLock;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock as TokioRwLock;
use tracing::{info, warn};

/// 将 ProxyAdapter 包装为实现 Proxy trait（增加 alive/alive_for_url 等方法）。
///
/// `inner` 是主出口（silverq-auto 选中的节点），`direct` 是 DIRECT 兜底：
/// 当 `inner` 的 dial 失败时自动回退到 `direct`，避免节点挂掉时连本地
/// 网关 / DNS 都打不通（v0.2.0 引入的断网防护）。
struct ProxyWrapper {
    inner: Arc<dyn ProxyAdapter>,
    direct: Arc<dyn ProxyAdapter>,
}

impl ProxyWrapper {
    fn new(inner: Arc<dyn ProxyAdapter>, direct: Arc<dyn ProxyAdapter>) -> Self {
        Self { inner, direct }
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
        match self.inner.dial_tcp(metadata).await {
            Ok(c) => Ok(c),
            Err(e) => {
                warn!(
                    err = %e,
                    tag = self.inner.name(),
                    "primary proxy dial_tcp failed, falling back to DIRECT"
                );
                self.direct.dial_tcp(metadata).await
            }
        }
    }
    async fn dial_udp(&self, metadata: &Metadata) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
        match self.inner.dial_udp(metadata).await {
            Ok(c) => Ok(c),
            Err(e) => {
                warn!(
                    err = %e,
                    tag = self.inner.name(),
                    "primary proxy dial_udp failed, falling back to DIRECT"
                );
                self.direct.dial_udp(metadata).await
            }
        }
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
}

impl TunConfig {
    pub fn from_effective(eff: &Effective) -> Self {
        Self {
            device: eff.tun_device.clone(),
            mtu: eff.tun_mtu,
            auto_route: eff.tun_auto_route,
            fake_ip_cidr: eff.tun_fake_ip_cidr.clone(),
            exclude_cidrs: eff.tun_exclude_cidrs.clone(),
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
    /// 当前已注册到 Tunnel 的 "silverq-auto"：tag + 注册时使用的 adapter。
    /// reload 会重建同 tag 的 adapter（新 Arc），光比 tag 发现不了 →
    /// TUN 出口会僵在旧配置上，热加载失效（见 sync_proxies）。
    current_auto: RwLock<Option<(String, Arc<dyn ProxyAdapter>)>>,
    /// 共享的 DIRECT 适配器：既作为 "silverq-auto" 的 dial 兜底，
    /// 又作为 "DIRECT" 注册进 Tunnel 供私网 / 用户排除 CIDR 规则路由。
    /// 单实例（Arc 共享），避免重复建 DirectAdapter。
    direct: Arc<dyn ProxyAdapter>,
    /// 用户配置的不走 TUN 的 CIDR（来自 `[tun].exclude_cidrs`），
    /// 在 `init_rules` 时生成 IpCidrRule → DIRECT。
    exclude_cidrs: Vec<String>,
}

impl TunRuntime {
    #[allow(clippy::too_many_arguments)]
    fn new(
        registry: Registry,
        selection: SharedSelection,
        fake_ip_cidr: Option<String>,
        exclude_cidrs: Vec<String>,
    ) -> Result<Self, String> {
        // fake-IP 池必须装进 resolver：TunRouteScope::FakeIp 的路由范围与 DNS
        // 劫持网关都取自 resolver.fake_ip_v4_net()，不装池子 → auto_route 不装
        // 路由、DNS 也无法合成假地址，TUN 静默不接任何流量。
        let fake_ip_range = fake_ip_cidr
            .as_deref()
            .and_then(|s| ipnet::Ipv4Net::from_str(s).ok())
            .unwrap_or_else(|| ipnet::Ipv4Net::new(Ipv4Addr::new(198, 18, 0, 0), 15).unwrap());

        let mut resolver = Resolver::new(
            vec![], // upstream DNS（留空，后续可通过配置添加）
            vec![], // hosts
            DnsMode::FakeIp,
            meow_trie::DomainTrie::new(),
            false, // use_hosts
        );
        // ponytail: 内存 LRU 容量 4096（与 meow 的 DnsCache 默认同量级）；
        // 池子本身按 CIDR 范围循环分配，LRU 只是 host↔ip 映射的淘汰上限。
        let store = Arc::new(MemoryStore::new(4096)) as Arc<dyn Store>;
        let pool = Pool::new(ipnet::IpNet::V4(fake_ip_range), store)
            .map_err(|e| format!("fake-ip pool init failed: {e}"))?;
        resolver.set_fakeip_v4(Arc::new(pool));
        let resolver = Arc::new(resolver);

        let tunnel = Tunnel::new(resolver);
        tunnel.set_mode(TunnelMode::Rule);
        tunnel.spawn_background_tasks();

        // 单一 DirectAdapter 实例，Arc 共享给 silverq-auto 兜底与 "DIRECT" 规则出口。
        let direct: Arc<dyn ProxyAdapter> = Arc::new(meow_proxy::DirectAdapter::new());

        Ok(Self {
            tunnel,
            registry,
            selection,
            current_auto: RwLock::new(None),
            direct,
            exclude_cidrs,
        })
    }

    /// 同步 "silverq-auto" 到 Tunnel：取 selection 首个节点，与上次注册的
    /// (tag, adapter) 比较，变了才重建 Tunnel 的 proxies map。
    async fn sync_proxies(&self) {
        let selection = self.selection.read().await;
        let registry = self.registry.read();

        let new_tag = selection.first().cloned();
        // registry 会因 reload 重建（同 tag 换新 Arc），所以期望状态要带上
        // adapter 本身，不能只比 tag —— 否则 reload 后出口停在旧节点配置上。
        let new = match new_tag.as_deref() {
            Some(tag) => registry.get(tag).map(|a| (tag.to_string(), Arc::clone(a))),
            None => None,
        };

        let mut current = self.current_auto.write();
        let needs_update = match (&*current, &new) {
            (Some((old_tag, old_arc)), Some((new_tag, new_arc))) => {
                old_tag != new_tag || !Arc::ptr_eq(old_arc, new_arc)
            }
            (None, None) => false,
            _ => true,
        };
        if !needs_update {
            return;
        }

        let mut proxies = HashMap::new();

        // 始终注册 "DIRECT"：既让私网 / 用户排除 CIDR 规则能按名路由，
        // 又保证空 selection 时本地流量仍可达（防断网安全网）。
        // 两字段都指向同一个共享 DirectAdapter（self.direct）。
        let direct_wrapped = Arc::new(ProxyWrapper::new(
            Arc::clone(&self.direct),
            Arc::clone(&self.direct),
        )) as Arc<dyn Proxy>;
        proxies.insert(SmolStr::new("DIRECT"), direct_wrapped);

        match new {
            Some((tag, adapter)) => {
                let wrapped = Arc::new(ProxyWrapper::new(
                    Arc::clone(&adapter),
                    Arc::clone(&self.direct),
                )) as Arc<dyn Proxy>;
                proxies.insert(SmolStr::new("silverq-auto"), wrapped);
                info!(tag = %tag, "TUN silverq-auto proxy updated");
                *current = Some((tag, adapter));
            }
            None => {
                // 没有可用节点（空 selection，或 tag 尚未在 registry 里）：
                // 移除 silverq-auto，只留 DIRECT。meow 的规则引擎对缺失的
                // adapter 名自动回退 DIRECT，故 FinalRule 仍能放行流量。
                *current = None;
                info!("TUN silverq-auto proxy cleared (no available nodes)");
            }
        }

        drop(current);
        // proxies 至少含 "DIRECT"，永不为空。
        self.tunnel.update_proxies(proxies);
    }

    /// 初始化规则（首次启动时调用）
    ///
    /// 规则按 first-match-wins 求值，顺序：
    /// 1. 私网安全网（硬编码 5 段）→ DIRECT
    /// 2. 用户 `[tun].exclude_cidrs` → DIRECT
    /// 3. FinalRule → silverq-auto（兜底）
    ///
    /// 不用 GEOIP，避免依赖 GeoIP 数据库。
    fn init_rules(&self) {
        let mut rules: Vec<Box<dyn meow_common::Rule>> = Vec::new();

        // 私网安全网：无论用户怎么配，这些段永远走 DIRECT（防断网）。
        const PRIVATE_NETS: &[&str] = &[
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "127.0.0.0/8",
            "169.254.0.0/16",
        ];
        for cidr in PRIVATE_NETS {
            match IpCidrRule::new(cidr, "DIRECT", false, false) {
                Ok(r) => rules.push(Box::new(r)),
                // 硬编码常量理论不会错，但即便错也只跳过这一条，不阻断 TUN 启动。
                Err(e) => warn!(cidr = cidr, err = %e, "private-network CIDR invalid, skipped"),
            }
        }

        // 用户排除 CIDR：非法的逐条 warn 并跳过，不让一条坏配置炸掉整个 TUN。
        for cidr in &self.exclude_cidrs {
            match IpCidrRule::new(cidr, "DIRECT", false, false) {
                Ok(r) => rules.push(Box::new(r)),
                Err(e) => warn!(cidr = %cidr, err = %e, "invalid exclude_cidr skipped"),
            }
        }

        // 兜底：其余流量走 silverq-auto。
        rules.push(Box::new(FinalRule::new("silverq-auto")));
        self.tunnel.update_rules(rules);
    }
}

/// 启动 TUN 数据面
pub async fn run(
    config: TunConfig,
    registry: Registry,
    selection: SharedSelection,
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
        Some(fake_ip_cidr.clone()),
        config.exclude_cidrs.clone(),
    )?);

    // 初始化规则
    runtime.init_rules();

    // 启动 proxies 同步任务（每 5 秒同步一次 selection 变化）
    let sync_runtime = Arc::clone(&runtime);
    let sync_task = tokio::spawn(async move {
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

    // readiness 只用于日志：listener 就绪时记一条，失败时不挡路。
    // 原实现在 run() 之后才 await ready_rx —— run 提前出错时 tx 被 drop、
    // ready_rx 永久挂起，错误既不返回也不被看见（tun 任务看起来还活着）。
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<TunReady>();
    let listener = listener.with_readiness_signal(ready_tx);
    tokio::spawn(async move {
        if ready_rx.await.is_ok() {
            info!("TUN 设备就绪");
        }
    });

    let result = listener.run().await;
    // listener 退出后停掉同步循环：否则它会持着 Arc<TunRuntime>（连同
    // tunnel / registry / selection）一直空跑到进程退出。
    sync_task.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::FileConfig;
    use meow_common::MeowError;
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
    /// 两字段都指向同一个 DirectAdapter（与生产代码里 "DIRECT" 注册一致）。
    fn direct_wrapper() -> ProxyWrapper {
        let inner: Arc<dyn ProxyAdapter> = Arc::new(DirectAdapter::new());
        let direct: Arc<dyn ProxyAdapter> = Arc::new(DirectAdapter::new());
        ProxyWrapper::new(inner, direct)
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
    }

    #[test]
    fn test_tun_config_from_effective_custom() {
        let mut eff = default_eff();
        eff.tun_device = Some("utun9".into());
        eff.tun_mtu = Some(1400);
        eff.tun_auto_route = false;
        eff.tun_fake_ip_cidr = Some("10.10.0.0/16".into());
        eff.tun_exclude_cidrs = vec!["1.2.3.0/24".into(), "9.9.9.0/24".into()];

        let tc = TunConfig::from_effective(&eff);
        assert_eq!(tc.device.as_deref(), Some("utun9"));
        assert_eq!(tc.mtu, Some(1400));
        assert!(!tc.auto_route);
        assert_eq!(tc.fake_ip_cidr.as_deref(), Some("10.10.0.0/16"));
        assert_eq!(
            tc.exclude_cidrs,
            vec!["1.2.3.0/24".to_string(), "9.9.9.0/24".to_string()]
        );
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
        };
        let err = run(config, empty_registry(), Arc::new(TokioRwLock::new(vec![])))
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
        // init_rules 现在注册「5 私网 + 用户 exclude_cidrs + FinalRule」。
        // 这里 exclude_cidrs 为空，所以应是 6 条，且最后一条是 FinalRule→silverq-auto。
        // （首条匹配语义下 FinalRule 永远在末尾兜底。）
        let rt = TunRuntime::new(
            empty_registry(),
            Arc::new(TokioRwLock::new(vec![])),
            Some("198.18.0.0/15".into()),
            vec![],
        )
        .expect("fake-ip pool init");
        rt.init_rules();
        let snap = rt.tunnel.route_snapshot();
        assert!(!snap.rules.is_empty(), "init_rules 至少注册一条 FinalRule");
        let last = snap.rules.last().expect("至少一条规则");
        assert_eq!(
            last.adapter(),
            "silverq-auto",
            "末尾 FinalRule 应指向 silverq-auto"
        );
        assert_eq!(last.payload(), "", "FinalRule payload 为空");
    }

    #[tokio::test]
    async fn test_sync_proxies_updates_on_selection_change() {
        // registry 放一个 direct adapter，selection 指向它 → sync 应把它包成
        // "silverq-auto" 注册进 Tunnel，并更新 current_auto。
        let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
        reg.insert("direct-a".into(), Arc::new(DirectAdapter::new()));
        let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

        let rt = TunRuntime::new(registry, selection, Some("198.18.0.0/15".into()), vec![])
            .expect("fake-ip pool init");

        assert!(
            rt.tunnel.proxy("silverq-auto").is_none(),
            "sync 前不应有 silverq-auto"
        );
        rt.sync_proxies().await;
        assert!(
            rt.tunnel.proxy("silverq-auto").is_some(),
            "sync 后应注册 silverq-auto"
        );
        let cur = rt.current_auto.read();
        assert_eq!(
            cur.as_ref().map(|(t, _)| t.as_str()),
            Some("direct-a"),
            "current_auto 应记下当前 tag"
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

        let rt = TunRuntime::new(registry, selection, Some("198.18.0.0/15".into()), vec![])
            .expect("fake-ip pool init");
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
        // 空 selection → 期望状态变 None：sync 会调 update_proxies 把 Tunnel 里的
        // silverq-auto 移除（只留 DIRECT），meow 规则引擎对缺失 adapter 自动回退 DIRECT。
        let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
        reg.insert("direct-a".into(), Arc::new(DirectAdapter::new()));
        let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

        let rt = TunRuntime::new(
            registry,
            selection.clone(),
            Some("198.18.0.0/15".into()),
            vec![],
        )
        .expect("fake-ip pool init");
        rt.sync_proxies().await;
        {
            let cur = rt.current_auto.read();
            assert_eq!(cur.as_ref().map(|(t, _)| t.as_str()), Some("direct-a"));
        }

        *selection.write().await = vec![];
        rt.sync_proxies().await;
        assert!(
            rt.current_auto.read().is_none(),
            "空 selection 后 current_auto 应被清空"
        );
        assert!(
            rt.tunnel.proxy("silverq-auto").is_none(),
            "空 selection 后 Tunnel 里的 silverq-auto 应被移除（回退 DIRECT）"
        );
    }

    #[tokio::test]
    async fn test_sync_proxies_reloads_rebuilt_adapter() {
        // reload 会用新 Arc 重建同 tag 的 adapter（ctl::do_reload 的行为）。
        // 只比 tag 发现不了变化 → TUN 出口会僵在旧 adapter 上；sync 必须检测到
        // 指针变化并重建路由表（snap2 != snap1）。
        let reg: HashMap<String, Arc<dyn ProxyAdapter>> = [(
            "direct-a".into(),
            Arc::new(DirectAdapter::new()) as Arc<dyn ProxyAdapter>,
        )]
        .into_iter()
        .collect();
        let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

        let rt = TunRuntime::new(
            registry.clone(),
            selection,
            Some("198.18.0.0/15".into()),
            vec![],
        )
        .expect("fake-ip pool init");
        rt.sync_proxies().await;
        let snap1 = rt.tunnel.route_snapshot();

        // 模拟 reload：同 tag 换一个新 adapter。
        registry
            .write()
            .insert("direct-a".into(), Arc::new(DirectAdapter::new()));
        rt.sync_proxies().await;
        let snap2 = rt.tunnel.route_snapshot();

        assert!(
            !Arc::ptr_eq(&snap1, &snap2),
            "reload 重建 adapter 后应重建 route table，不能僵在旧 adapter"
        );
    }

    #[tokio::test]
    async fn test_sync_proxies_skips_when_tag_missing_from_registry() {
        // selection 指向 registry 里还没有的 tag（节点被移除 / 尚未构建）：
        // 期望状态与当前都是 None → 跳过，不每 5s 空转重建路由表。
        let registry: Registry = empty_registry();
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["not-there".into()]));
        let rt = TunRuntime::new(registry, selection, Some("198.18.0.0/15".into()), vec![])
            .expect("fake-ip pool init");

        rt.sync_proxies().await;
        let snap1 = rt.tunnel.route_snapshot();
        rt.sync_proxies().await;
        let snap2 = rt.tunnel.route_snapshot();
        assert!(
            Arc::ptr_eq(&snap1, &snap2),
            "tag 缺失时不应反复重建 route table"
        );
        assert!(rt.current_auto.read().is_none());
    }

    // ---- C: DIRECT fallback（v0.2.0 防断网安全网）----

    /// dial 总是失败的假 adapter，用于验证 `ProxyWrapper` 的 DIRECT 兜底：
    /// 主出口 dial 失败时应自动回退到 direct，而非把错误透传给上层。
    struct FailingAdapter {
        health: ProxyHealth,
    }

    #[async_trait::async_trait]
    impl ProxyAdapter for FailingAdapter {
        fn name(&self) -> &str {
            "failing"
        }
        fn adapter_type(&self) -> AdapterType {
            AdapterType::Socks5
        }
        fn addr(&self) -> &str {
            ""
        }
        fn support_udp(&self) -> bool {
            true
        }
        async fn dial_tcp(&self, _metadata: &Metadata) -> meow_common::Result<Box<dyn ProxyConn>> {
            Err(MeowError::Io(std::io::Error::other(
                "mock dial_tcp failure",
            )))
        }
        async fn dial_udp(
            &self,
            _metadata: &Metadata,
        ) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
            Err(MeowError::Io(std::io::Error::other(
                "mock dial_udp failure",
            )))
        }
        fn health(&self) -> &ProxyHealth {
            &self.health
        }
    }

    #[tokio::test]
    async fn test_proxy_wrapper_falls_back_to_direct_on_dial_failure() {
        // inner = 总是失败的假 adapter；direct = 真 DirectAdapter。
        // dial 一个本地真监听端口：inner 必失败 → 回退 direct → 连上 → Ok。
        // 能拿到 Ok 就证明兜底生效（错误没被透传）。
        let failing: Arc<dyn ProxyAdapter> = Arc::new(FailingAdapter {
            health: ProxyHealth::new(),
        });
        let direct: Arc<dyn ProxyAdapter> = Arc::new(DirectAdapter::new());
        let wrapper = ProxyWrapper::new(failing, direct);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 持续 accept，避免 backlog 满导致 connect 被拒（虽单连通常不会）。
        let accept_handle = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });

        let metadata = Metadata {
            dst_ip: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
            dst_port: addr.port(),
            ..Default::default()
        };

        let result = wrapper.dial_tcp(&metadata).await;
        accept_handle.abort();
        assert!(
            result.is_ok(),
            "inner dial 失败应回退 DIRECT 并成功，实际: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_init_rules_includes_private_network_excludes() {
        // 5 私网(硬编码) + 1 用户 CIDR + 1 FinalRule = 7 条，顺序固定。
        let rt = TunRuntime::new(
            empty_registry(),
            Arc::new(TokioRwLock::new(vec![])),
            Some("198.18.0.0/15".into()),
            vec!["224.0.0.0/4".into()],
        )
        .expect("fake-ip pool init");
        rt.init_rules();
        let snap = rt.tunnel.route_snapshot();
        assert_eq!(snap.rules.len(), 7, "应 = 5 私网 + 1 用户 + 1 final");

        let private = [
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "127.0.0.0/8",
            "169.254.0.0/16",
        ];
        for (i, cidr) in private.iter().enumerate() {
            assert_eq!(snap.rules[i].payload(), *cidr, "私网规则顺序/内容 @ {i}");
            assert_eq!(
                snap.rules[i].adapter(),
                "DIRECT",
                "私网规则应路由 DIRECT @ {i}"
            );
        }
        assert_eq!(snap.rules[5].payload(), "224.0.0.0/4", "第 6 条是用户 CIDR");
        assert_eq!(snap.rules[5].adapter(), "DIRECT");
        assert_eq!(snap.rules[6].payload(), "", "末尾 FinalRule payload 为空");
        assert_eq!(snap.rules[6].adapter(), "silverq-auto");
    }

    #[tokio::test]
    async fn test_init_rules_skips_malformed_user_cidr() {
        // 非法 CIDR 应被静默跳过（warn），不 panic、不阻断其余规则。
        // 5 私网 + 1 final = 6（坏的那条不计）。
        let rt = TunRuntime::new(
            empty_registry(),
            Arc::new(TokioRwLock::new(vec![])),
            Some("198.18.0.0/15".into()),
            vec!["not-a-cidr".into()],
        )
        .expect("fake-ip pool init");
        rt.init_rules();
        let snap = rt.tunnel.route_snapshot();
        assert_eq!(snap.rules.len(), 6, "非法 CIDR 被跳过：5 私网 + 1 final");
        // 末尾仍是 FinalRule，未被坏配置影响。
        assert_eq!(snap.rules.last().unwrap().adapter(), "silverq-auto");
    }

    #[tokio::test]
    async fn test_direct_proxy_registered_in_tunnel() {
        // sync_proxies 后 "DIRECT" 应可按名解析（规则路由私网/排除 CIDR 依赖它）。
        let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
        reg.insert("direct-a".into(), Arc::new(DirectAdapter::new()));
        let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
        let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

        let rt = TunRuntime::new(registry, selection, Some("198.18.0.0/15".into()), vec![])
            .expect("fake-ip pool init");
        assert!(rt.tunnel.proxy("DIRECT").is_none(), "sync 前不应有 DIRECT");
        rt.sync_proxies().await;
        assert!(
            rt.tunnel.proxy("DIRECT").is_some(),
            "sync 后应注册 DIRECT 供规则路由"
        );
    }

    // ---- B: root-gated 手动 smoke 测试（需要 root / CAP_NET_ADMIN） ----
    //
    // 本地跑（CI 不跑 —— `#[ignore]`，且 runner 无 root）：
    //   sudo cargo test --features meow-tun --bin silverq -- --ignored --test-threads=1
    //
    // 测试在 lib 里，但这些 smoke 需要直接调 `tun::run`（私有函数 + root 权限），
    // 放在本模块内最直接（同模块可直接调 `run`）。详见 README「TUN 手动测试」。

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
        };
        let mut handle = tokio::spawn(async move {
            let _ = run(config, empty_registry(), Arc::new(TokioRwLock::new(vec![]))).await;
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
        };
        let res = tokio::time::timeout(
            Duration::from_secs(3),
            run(config, empty_registry(), Arc::new(TokioRwLock::new(vec![]))),
        )
        .await;
        assert!(res.is_ok(), "run 应在 3s 内返回，而非永久挂起");
    }
}
