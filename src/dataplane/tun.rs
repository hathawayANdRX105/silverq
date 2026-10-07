//! TUN 透明代理数据面 —— 基于 meow-listener 的 listener-tun 实现。
//!
//! 功能：创建 TUN 设备 → 接管 fake-IP 范围路由 → 所有 fake-IP 流量进 TUN
//! → 经 meow Tunnel 引擎按规则分发 → 复用 silverq 的 ProxyAdapter 做出站。

#![cfg(all(feature = "meow", feature = "meow-listener"))]

use crate::config::settings::Effective;
use crate::dataplane::inbound::{DialTuning, SharedTuning};
use crate::proxy::meow::Registry;

use meow_common::{
    AdapterType, DelayHistory, DnsMode, MeowError, Metadata, Proxy, ProxyAdapter, ProxyConn,
    ProxyHealth, ProxyPacketConn, TunnelMode,
};
use meow_dns::fakeip::{MemoryStore, Pool, Skipper, SkipperMode, Store};
use meow_dns::resolver::Resolver;
use meow_listener::tun::{TunListener, TunListenerConfig, TunReady, TunRouteScope};
use meow_rules::final_rule::FinalRule;
use meow_rules::ipcidr::IpCidrRule;
use meow_tunnel::Tunnel;
use parking_lot::RwLock;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock as TokioRwLock;
use tracing::{info, warn};

/// TUN 出口代理包装：候选链 + DIRECT 兜底。
///
/// `candidates` 按 selection 顺序排列，主候选 dial 失败时逐个往下试，
/// 全部失败再退 DIRECT。与 SOCKS5 入站（inbound.rs 的 fallback_attempts）
/// 对齐——否则 TUN 只用 selection[0] 单点，节点池半死时每 5 秒切出口
/// 都会卡一批连接（实测 gstatic 0.07s↔20s 抖动）。
///
/// 每个候选 dial 都包 `timeout(dt.dial())`：黑洞节点（SYN 丢弃、不回 RST）
/// 的 dial 永不返回，没有超时则候选链永远不前进、请求挂到客户端超时
/// （2026-09-20 实验室复现：TUN 每请求挂 10s+，同期 SOCKS 路径一直有超时）。
pub struct ProxyWrapper {
    candidates: Vec<Arc<dyn ProxyAdapter>>,
    direct: Arc<dyn ProxyAdapter>,
    tuning: SharedTuning,
    /// TUN resolver 的 fake-IP store：DIRECT 兜底时反查 fake-IP → 域名，
    /// 再用真实上游 DNS 解出真身 IP（见 dial_direct_tcp/dial_direct_udp）。
    /// SOCKS 流量天生不带假地址，没有这层也能直连；TUN 必须反查。
    fakeip_store: Option<Arc<dyn Store>>,
}

impl ProxyWrapper {
    /// `candidates` 至少含一个元素；空时等价于纯 DIRECT（由 sync_proxies
    /// 保证「空 selection」路径构造 direct-only wrapper，见下方）。
    pub fn new(
        candidates: Vec<Arc<dyn ProxyAdapter>>,
        direct: Arc<dyn ProxyAdapter>,
        tuning: SharedTuning,
        fakeip_store: Option<Arc<dyn Store>>,
    ) -> Self {
        Self {
            candidates,
            direct,
            tuning,
            fakeip_store,
        }
    }

    fn primary_name(&self) -> &str {
        self.candidates
            .first()
            .map(|a| a.name())
            .unwrap_or("DIRECT")
    }

    /// fake-IP 反查：store 里有的假地址换回域名，没有的原样返回。
    pub fn real_host_of(&self, metadata: &Metadata) -> Option<String> {
        let store = self.fakeip_store.as_ref()?;
        let ip = metadata.dst_ip?;
        store.get_by_ip(ip).map(|h| h.to_string())
    }

    /// DIRECT 兜底（TCP）：dst_ip 是 fake-IP 时先反查域名、经真实上游 DNS
    /// 解出真身再拨。直接拨假地址只会路由回 TUN 秒失败（DirectAdapter 见
    /// dst_ip 就拨它，meow-proxy direct.rs resolve_targets 第 1 步）；
    /// 与 inbound.rs 的 DirectDial::Host 路径等价——实验室实测全候选死时
    /// SOCKS 能救回 gstatic，反查前 TUN 不能。
    ///
    /// 信任边界：解析结果落私网/回环时不拨（DNS 污染场景），保持原
    /// metadata 让请求失败——与 inbound.rs 的 is_local_target 同源。
    async fn dial_direct_tcp(
        &self,
        metadata: &Metadata,
        timeout: Duration,
    ) -> meow_common::Result<Box<dyn ProxyConn>> {
        let md = match self.real_host_of(metadata) {
            Some(host) => match crate::proxy::dns::resolve_host(&host).await {
                Some(ip) if !crate::dataplane::inbound::is_local_target(&ip.to_string()) => {
                    Metadata {
                        dst_ip: Some(ip),
                        ..metadata.clone()
                    }
                }
                _ => metadata.clone(),
            },
            None => metadata.clone(),
        };
        match tokio::time::timeout(timeout, self.direct.dial_tcp(&md)).await {
            Ok(r) => r,
            Err(_) => Err(MeowError::Io(std::io::Error::other(format!(
                "DIRECT fallback dial timed out after {timeout:?}"
            )))),
        }
    }

    /// DIRECT 兜底（UDP）：同 dial_direct_tcp 的反查与信任边界逻辑。
    async fn dial_direct_udp(
        &self,
        metadata: &Metadata,
        timeout: Duration,
    ) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
        let md = match self.real_host_of(metadata) {
            Some(host) => match crate::proxy::dns::resolve_host(&host).await {
                Some(ip) if !crate::dataplane::inbound::is_local_target(&ip.to_string()) => {
                    Metadata {
                        dst_ip: Some(ip),
                        ..metadata.clone()
                    }
                }
                _ => metadata.clone(),
            },
            None => metadata.clone(),
        };
        match tokio::time::timeout(timeout, self.direct.dial_udp(&md)).await {
            Ok(r) => r,
            Err(_) => Err(MeowError::Io(std::io::Error::other(format!(
                "DIRECT fallback dial timed out after {timeout:?}"
            )))),
        }
    }
}

#[async_trait::async_trait]
impl ProxyAdapter for ProxyWrapper {
    fn name(&self) -> &str {
        self.primary_name()
    }
    fn adapter_type(&self) -> AdapterType {
        self.candidates
            .first()
            .map(|a| a.adapter_type())
            .unwrap_or(AdapterType::Direct)
    }
    fn addr(&self) -> &str {
        self.candidates.first().map(|a| a.addr()).unwrap_or("")
    }
    fn support_udp(&self) -> bool {
        self.candidates
            .first()
            .map(|a| a.support_udp())
            .unwrap_or(true)
    }
    async fn dial_tcp(&self, metadata: &Metadata) -> meow_common::Result<Box<dyn ProxyConn>> {
        // 每 dial 现取快照：面板热改 timeout_ms / fallback_attempts 对新建
        // 连接即时生效（与 inbound.rs 的 SOCKS 路径同源）。
        //
        // 已知边界（不在本修复范围）：dial 超时只覆盖建连。「连得上但不下
        // 蛋」的黑洞节点（AEAD 协议 dial_tcp 立刻返回 Ok）在 TUN 路径没有
        // first_response 防护——relay 归 meow 引擎所有，插不进去；SOCKS 路径
        // 有（inbound.rs relay 的首读超时）。
        let dt = DialTuning::snapshot(&self.tuning.read());
        let dial_timeout = dt.dial();
        for (i, adapter) in self.candidates.iter().enumerate() {
            match tokio::time::timeout(dial_timeout, adapter.dial_tcp(metadata)).await {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(e)) => {
                    // 非末位候选的失败是运维信号（记录主候选方便定位）；
                    // 只有全链失败才升级为 warn 并兜底 DIRECT。
                    if i + 1 < self.candidates.len() {
                        warn!(
                            err = %e,
                            tag = adapter.name(),
                            next = self.candidates[i + 1].name(),
                            "dial_tcp failed, trying next candidate"
                        );
                    } else {
                        warn!(
                            err = %e,
                            tag = adapter.name(),
                            "all candidates failed, falling back to DIRECT"
                        );
                    }
                }
                Err(_) => {
                    // 黑洞节点：TCP SYN 被丢弃时 dial 永不返回。这层超时是
                    // 候选链能继续前进的唯一前提（裸 await 会挂死请求）。
                    if i + 1 < self.candidates.len() {
                        warn!(
                            tag = adapter.name(),
                            next = self.candidates[i + 1].name(),
                            timeout = ?dial_timeout,
                            "dial 超时，换下一个候选"
                        );
                    } else {
                        warn!(
                            tag = adapter.name(),
                            timeout = ?dial_timeout,
                            "all candidates failed (dial timeout), falling back to DIRECT"
                        );
                    }
                }
            }
        }
        // DIRECT 兜底同样包超时：黑洞场景下直连也可能只发 SYN 不收 ACK。
        // 超时取 max(dial, first_response)，与 inbound.rs 的 direct_timeout 一致。
        // fake-IP 目标先反查真身（见 dial_direct_tcp）。
        let t = std::cmp::max(dial_timeout, dt.first_response());
        self.dial_direct_tcp(metadata, t).await
    }
    async fn dial_udp(&self, metadata: &Metadata) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
        let dt = DialTuning::snapshot(&self.tuning.read());
        let dial_timeout = dt.dial();
        for (i, adapter) in self.candidates.iter().enumerate() {
            match tokio::time::timeout(dial_timeout, adapter.dial_udp(metadata)).await {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(e)) => {
                    if i + 1 < self.candidates.len() {
                        warn!(
                            err = %e,
                            tag = adapter.name(),
                            next = self.candidates[i + 1].name(),
                            "dial_udp failed, trying next candidate"
                        );
                    } else {
                        warn!(
                            err = %e,
                            tag = adapter.name(),
                            "all candidates failed, falling back to DIRECT"
                        );
                    }
                }
                Err(_) => {
                    if i + 1 < self.candidates.len() {
                        warn!(
                            tag = adapter.name(),
                            next = self.candidates[i + 1].name(),
                            timeout = ?dial_timeout,
                            "dial 超时，换下一个候选"
                        );
                    } else {
                        warn!(
                            tag = adapter.name(),
                            timeout = ?dial_timeout,
                            "all candidates failed (dial timeout), falling back to DIRECT"
                        );
                    }
                }
            }
        }
        let t = std::cmp::max(dial_timeout, dt.first_response());
        self.dial_direct_udp(metadata, t).await
    }
    fn health(&self) -> &ProxyHealth {
        self.candidates
            .first()
            .map(|a| a.health())
            .unwrap_or(self.direct.health())
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

/// `current_auto` 快照：(整条候选链的名字, 注册时首个 adapter 的 Arc)。
/// 名字列表用于检测 selection 中段变化，Arc 指针用于检测 reload 重建。
pub type CurrentAuto = Option<(Vec<String>, Arc<dyn ProxyAdapter>)>;

/// TUN 运行时状态（用于热更新 proxies）
pub struct TunRuntime {
    pub tunnel: Tunnel,
    registry: Registry,
    selection: SharedSelection,
    /// 共享调参：ProxyWrapper 每个 dial 现取快照（timeout_ms 派生 dial 超时、
    /// fallback_attempts 截断候选链）。面板热改对 TUN 数据面即时生效。
    tuning: SharedTuning,
    /// 当前已注册到 Tunnel 的 "silverq-auto"：tag + 注册时使用的 adapter。
    /// reload 会重建同 tag 的 adapter（新 Arc），光比 tag 发现不了 →
    /// TUN 出口会僵在旧配置上，热加载失效（见 sync_proxies）。
    pub current_auto: RwLock<CurrentAuto>,
    /// 共享的 DIRECT 适配器：既作为 "silverq-auto" 的 dial 兜底，
    /// 又作为 "DIRECT" 注册进 Tunnel 供私网 / 用户排除 CIDR 规则路由。
    /// 单实例（Arc 共享），避免重复建 DirectAdapter。
    direct: Arc<dyn ProxyAdapter>,
    /// 用户配置的不走 TUN 的 CIDR（来自 `[tun].exclude_cidrs`），
    /// 在 `init_rules` 时生成 IpCidrRule → DIRECT。
    exclude_cidrs: Vec<String>,
    /// fake-IP store 句柄（与 resolver 内 Pool 共享同一 store）：
    /// ProxyWrapper 的 DIRECT 兜底靠它反查假地址 → 域名。
    fakeip_store: Option<Arc<dyn Store>>,
}

impl TunRuntime {
    /// 构造 TUN 运行时：装配 fake-IP 池、resolver 与 Tunnel；pub 供 tests/tun.rs 白盒断言。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Registry,
        selection: SharedSelection,
        tuning: SharedTuning,
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

        // 真实上游 DNS：fake-IP skipper 命中的域名（国内表）经它做真实解析。
        // 留空的话命中域名的查询无人可答。与 proxy::dns 共用同一组上游
        // （223.5.5.5 / 119.29.29.29，SILVERQ_RESOLVE_UPSTREAMS 可覆盖）。
        let upstreams: Vec<SocketAddr> = crate::proxy::dns::upstreams()
            .into_iter()
            .map(|ip| SocketAddr::new(ip, 53))
            .collect();
        let mut resolver = Resolver::new(
            upstreams,
            vec![], // hosts
            DnsMode::FakeIp,
            meow_trie::DomainTrie::new(),
            false, // use_hosts
        );
        // ponytail: 内存 LRU 容量 4096（与 meow 的 DnsCache 默认同量级）；
        // 池子本身按 CIDR 范围循环分配，LRU 只是 host↔ip 映射的淘汰上限。
        let store = Arc::new(MemoryStore::new(4096)) as Arc<dyn Store>;
        let pool = Pool::new(ipnet::IpNet::V4(fake_ip_range), Arc::clone(&store))
            .map_err(|e| format!("fake-ip pool init failed: {e}"))?;
        resolver.set_fakeip_v4(Arc::new(pool));
        // 国内域名 fake-IP 旁路（BlackList：命中即真实解析）——国内流量在
        // DNS 层就拿真实 IP，不落 198.18/15 路由，TUN 根本不碰它们。
        // 表缺失 = 空 skipper = 全部照旧走 fake-IP（安全降级）。
        let china_patterns =
            crate::proxy::dns::load_china_domains(&crate::proxy::dns::china_domains_path());
        if !china_patterns.is_empty() {
            resolver.set_fakeip_skipper(Skipper::new(&china_patterns, SkipperMode::BlackList));
        }
        tracing::info!(count = china_patterns.len(), "国内域名 fake-IP 旁路表加载");
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
            tuning,
            current_auto: RwLock::new(None),
            direct,
            exclude_cidrs,
            fakeip_store: Some(store),
        })
    }

    /// 同步 "silverq-auto" 到 Tunnel：取 selection 首个节点，与上次注册的
    /// (tag, adapter) 比较，变了才重建 Tunnel 的 proxies map。
    pub async fn sync_proxies(&self) {
        let selection = self.selection.read().await;
        let registry = self.registry.read();

        // 整条 selection 都是候选（不只是首个）：dial 时逐个尝试，与
        // SOCKS5 入站的 fallback_attempts 对齐。registry 可能缺条目
        // （reload 时序），按序收集存在的 adapter。
        let mut candidates: Vec<Arc<dyn ProxyAdapter>> = selection
            .iter()
            .filter_map(|tag| registry.get(tag).map(Arc::clone))
            .collect();
        // 截断到 fallback_attempts：此前漏了这步，TUN 实际试满整条 selection
        // （capacity=10）。每候选 dial 超时 timeout_ms，不截断 = 最坏 40s 才
        // 兜底；与 SOCKS 同 knob 同语义（队首 + 两个次优，可调）。
        let dt = DialTuning::snapshot(&self.tuning.read());
        candidates.truncate(dt.fallback_attempts);
        let new_names: Vec<String> = candidates.iter().map(|a| a.name().to_string()).collect();
        let new_tag = candidates.first().map(|a| a.name().to_string());

        let mut current = self.current_auto.write();
        // 只比队首 tag 会漏两种变化：selection 中段调整（队首不变）与
        // reload 重建同 tag adapter（Arc 换新）。整链名字 + 首个 adapter
        // 指针任一变化都必须重建，否则 TUN 出口僵在旧候选链上。
        let needs_update = match (&*current, candidates.first()) {
            (None, None) => false,
            (Some((old_names, old_first)), Some(first)) => {
                old_names != &new_names || !Arc::ptr_eq(old_first, first)
            }
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
            vec![Arc::clone(&self.direct)],
            Arc::clone(&self.direct),
            self.tuning.clone(),
            self.fakeip_store.clone(),
        )) as Arc<dyn Proxy>;
        proxies.insert(SmolStr::new("DIRECT"), direct_wrapped);

        match new_tag {
            Some(tag) => {
                let wrapped = Arc::new(ProxyWrapper::new(
                    candidates.clone(),
                    Arc::clone(&self.direct),
                    self.tuning.clone(),
                    self.fakeip_store.clone(),
                )) as Arc<dyn Proxy>;
                proxies.insert(SmolStr::new("silverq-auto"), wrapped);
                info!(tag = %tag, count = candidates.len(), "TUN silverq-auto proxy updated");
                *current = Some((new_names, candidates.into_iter().next().unwrap()));
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
    pub fn init_rules(&self) {
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
    tuning: SharedTuning,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!(
        device = config.device.as_deref().unwrap_or("auto"),
        auto_route = config.auto_route,
        "启动 TUN 透明代理"
    );

    // 解析 fake-ip CIDR
    // 非法配置必须前置拒绝：TunRuntime::new 里对解析失败静默回退默认段
    // （容错设计是给「未配置」用的，不该吞掉写错的值），而 TUN 设备创建
    // 需要 root —— 无特权环境下错误会被设备错误掩盖（回归测试锁定此序）。
    if let Some(cidr) = &config.fake_ip_cidr {
        if cidr.parse::<ipnet::Ipv4Net>().is_err() {
            return Err(format!("invalid fake_ip_cidr: {cidr}").into());
        }
    }
    let fake_ip_cidr = config
        .fake_ip_cidr
        .clone()
        .unwrap_or_else(|| "198.18.0.0/15".to_string());
    // 设备自身地址必须与 fake-IP 范围不相交：meow-listener 的 is_looping_dst
    // 把「目标落在设备子网内」的包当环路丢弃。若把整个 fake 范围设成设备
    // 子网，所有 fake-IP 流量会被静默 drop——DNS 劫持照常（UDP :53 在
    // 环路判定之前被拦截），但 TCP 握手成功后一发数据就 RST。
    // 与 meow-config 的默认 172.19.0.1/30 保持一致。
    let device_net: ipnet::Ipv4Net = "172.19.0.1/30"
        .parse()
        .expect("device address must be a valid CIDR");

    // 创建 TUN 运行时
    let runtime = Arc::new(TunRuntime::new(
        registry.clone(),
        selection.clone(),
        tuning,
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
        inet4_address: device_net, // 设备自身子网（与 fake 范围不相交，见上）
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
