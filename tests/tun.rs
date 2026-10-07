//! TUN 数据面单元测试（自 src/dataplane/tun.rs 迁入）：TunConfig 映射、ProxyWrapper 桩、TunRuntime 规则/同步。
#![cfg(all(feature = "meow", feature = "meow-listener"))]

use meow_common::{
    AdapterType, MeowError, Metadata, Proxy, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn,
};
use meow_dns::fakeip::{MemoryStore, Store};
use meow_proxy::direct::DirectAdapter;
use silverq::config::settings::{Effective, FileConfig, RuntimeTuning};
use silverq::dataplane::inbound::SharedTuning;
use silverq::dataplane::tun::{run, ProxyWrapper, SharedSelection, TunConfig, TunRuntime};
use silverq::proxy::meow::Registry;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock as TokioRwLock;

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
    ProxyWrapper::new(vec![inner], direct, test_tuning(), None)
}

/// 测试用共享调参：默认值（timeout_ms 4000 / fallback_attempts 3）。
/// 需要小超时的用例构造后原地改字段（RuntimeTuning 字段均 pub）。
fn test_tuning() -> SharedTuning {
    Arc::new(parking_lot::RwLock::new(RuntimeTuning::from_eff(
        &default_eff(),
    )))
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
    let err = run(
        config,
        empty_registry(),
        Arc::new(TokioRwLock::new(vec![])),
        test_tuning(),
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
    // init_rules 现在注册「5 私网 + 用户 exclude_cidrs + FinalRule」。
    // 这里 exclude_cidrs 为空，所以应是 6 条，且最后一条是 FinalRule→silverq-auto。
    // （首条匹配语义下 FinalRule 永远在末尾兜底。）
    let rt = TunRuntime::new(
        empty_registry(),
        Arc::new(TokioRwLock::new(vec![])),
        test_tuning(),
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
    reg.insert(
        "direct-a".into(),
        Arc::new(NamedStubAdapter {
            name: "direct-a",
            health: ProxyHealth::new(),
        }),
    );
    let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
    let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

    let rt = TunRuntime::new(
        registry,
        selection,
        test_tuning(),
        Some("198.18.0.0/15".into()),
        vec![],
    )
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
        cur.as_ref().map(|(n, _)| n[0].as_str()),
        Some("direct-a"),
        "current_auto 应记下当前候选链"
    );
}

#[tokio::test]
async fn test_sync_proxies_skipped_when_selection_unchanged() {
    // 同一 selection 第二次 sync：`*current == new_tag` → 不进更新分支，
    // 不重建 route table。`route_snapshot()` 返回的是 `Arc::clone` 自存储表，
    // 未更新时两次快照指向同一分配，指针相等即证明"跳过了"。
    let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
    reg.insert(
        "direct-a".into(),
        Arc::new(NamedStubAdapter {
            name: "direct-a",
            health: ProxyHealth::new(),
        }),
    );
    let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
    let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

    let rt = TunRuntime::new(
        registry,
        selection,
        test_tuning(),
        Some("198.18.0.0/15".into()),
        vec![],
    )
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
    reg.insert(
        "direct-a".into(),
        Arc::new(NamedStubAdapter {
            name: "direct-a",
            health: ProxyHealth::new(),
        }),
    );
    let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
    let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

    let rt = TunRuntime::new(
        registry,
        selection.clone(),
        test_tuning(),
        Some("198.18.0.0/15".into()),
        vec![],
    )
    .expect("fake-ip pool init");
    rt.sync_proxies().await;
    {
        let cur = rt.current_auto.read();
        assert_eq!(cur.as_ref().map(|(n, _)| n[0].as_str()), Some("direct-a"));
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
        test_tuning(),
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
async fn test_sync_proxies_truncates_to_fallback_attempts() {
    // 回归：候选链必须截断到 fallback_attempts（与 SOCKS 同 knob 同语义）。
    // 933bce6 之前漏了截断——TUN 实际试满整条 selection（capacity=10），
    // 加上 dial 超时后最坏 40s 才兜底，与配置语义不符。
    let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
    for name in ["a", "b", "c", "d", "e"] {
        reg.insert(
            name.into(),
            Arc::new(NamedStubAdapter {
                name,
                health: ProxyHealth::new(),
            }),
        );
    }
    let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
    let selection: SharedSelection = Arc::new(TokioRwLock::new(vec![
        "a".into(),
        "b".into(),
        "c".into(),
        "d".into(),
        "e".into(),
    ]));

    let mut t = RuntimeTuning::from_eff(&default_eff());
    t.fallback_attempts = 2;
    let tuning: SharedTuning = Arc::new(parking_lot::RwLock::new(t));

    let rt = TunRuntime::new(
        registry,
        selection,
        tuning,
        Some("198.18.0.0/15".into()),
        vec![],
    )
    .expect("fake-ip pool init");
    rt.sync_proxies().await;

    let cur = rt.current_auto.read();
    assert_eq!(
        cur.as_ref().map(|(n, _)| n.len()),
        Some(2usize),
        "5 个候选 + fallback_attempts=2 → 只注册队首 2 个"
    );
}

#[tokio::test]
async fn test_sync_proxies_updates_on_midchain_change() {
    // 回归：selection 队首不变、中段调整（[a] → [a,b]）也必须重建。
    // 旧实现只比队首 tag，候选链会僵在旧配置上（2026-09-19 事故日志里
    // selection 已切新、WARN 仍在按旧链逐个拨）。
    let mut reg: HashMap<String, Arc<dyn ProxyAdapter>> = HashMap::new();
    reg.insert(
        "direct-a".into(),
        Arc::new(NamedStubAdapter {
            name: "direct-a",
            health: ProxyHealth::new(),
        }),
    );
    let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
    let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

    let rt = TunRuntime::new(
        registry.clone(),
        selection.clone(),
        test_tuning(),
        Some("198.18.0.0/15".into()),
        vec![],
    )
    .expect("fake-ip pool init");
    rt.sync_proxies().await;
    let snap1 = rt.tunnel.route_snapshot();

    registry
        .write()
        .insert("direct-b".into(), Arc::new(DirectAdapter::new()));
    *selection.write().await = vec!["direct-a".into(), "direct-b".into()];
    rt.sync_proxies().await;
    let snap2 = rt.tunnel.route_snapshot();

    assert!(
        !Arc::ptr_eq(&snap1, &snap2),
        "队首不变但中段变化时应重建候选链"
    );
    let cur = rt.current_auto.read();
    assert_eq!(
        cur.as_ref().map(|(n, _)| n.len()),
        Some(2usize),
        "应记下整条链"
    );
}

#[tokio::test]
async fn test_sync_proxies_skips_when_tag_missing_from_registry() {
    // selection 指向 registry 里还没有的 tag（节点被移除 / 尚未构建）：
    // 期望状态与当前都是 None → 跳过，不每 5s 空转重建路由表。
    let registry: Registry = empty_registry();
    let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["not-there".into()]));
    let rt = TunRuntime::new(
        registry,
        selection,
        test_tuning(),
        Some("198.18.0.0/15".into()),
        vec![],
    )
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

/// 具名 stub：name 可指定。DirectAdapter::new() 的 name 恒为 "DIRECT"，
/// 验证 current_auto 按候选链记录时需要能带上注册键同名的 adapter。
struct NamedStubAdapter {
    name: &'static str,
    health: ProxyHealth,
}

#[async_trait::async_trait]
impl ProxyAdapter for NamedStubAdapter {
    fn name(&self) -> &str {
        self.name
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
        Err(MeowError::Io(std::io::Error::other("stub adapter")))
    }
    async fn dial_udp(
        &self,
        _metadata: &Metadata,
    ) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
        Err(MeowError::Io(std::io::Error::other("stub adapter")))
    }
    fn health(&self) -> &ProxyHealth {
        &self.health
    }
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
    let wrapper = ProxyWrapper::new(vec![failing], direct, test_tuning(), None);

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

/// dial 永远挂起的假 adapter：模拟黑洞节点（SYN 丢弃、dial 不返回）。
struct HangingAdapter {
    health: ProxyHealth,
}

#[async_trait::async_trait]
impl ProxyAdapter for HangingAdapter {
    fn name(&self) -> &str {
        "hanging"
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
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Err(MeowError::Io(std::io::Error::other("unreachable")))
    }
    async fn dial_udp(
        &self,
        _metadata: &Metadata,
    ) -> meow_common::Result<Box<dyn ProxyPacketConn>> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Err(MeowError::Io(std::io::Error::other("unreachable")))
    }
    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

fn loopback_metadata(port: u16) -> Metadata {
    Metadata {
        dst_ip: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
        dst_port: port,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_dial_timeout_advances_candidate_chain() {
    // 回归（2026-09-20 实验室复现）：候选[0] 是黑洞节点（dial 永不返回），
    // 旧实现裸 await → 候选链永远不前进 → 请求挂到客户端超时（实测 TUN
    // 每请求 10s+ 卡死，同期 SOCKS 路径有超时不受影响）。
    // 新实现：dial 超时后必须换到候选[1] 并成功。
    let mut t = RuntimeTuning::from_eff(&default_eff());
    t.timeout_ms = 50;
    let tuning: SharedTuning = Arc::new(parking_lot::RwLock::new(t));

    let hanging: Arc<dyn ProxyAdapter> = Arc::new(HangingAdapter {
        health: ProxyHealth::new(),
    });
    // 候选[1] 用真 DirectAdapter：连本地监听即成功。
    let direct: Arc<dyn ProxyAdapter> = Arc::new(DirectAdapter::new());
    // 兜底 direct 故意用挂起 adapter：若候选链没前进到 [1]，这扇门不开。
    let hanging_direct: Arc<dyn ProxyAdapter> = Arc::new(HangingAdapter {
        health: ProxyHealth::new(),
    });
    let wrapper = ProxyWrapper::new(vec![hanging, direct], hanging_direct, tuning, None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_handle = tokio::spawn(async move {
        loop {
            if listener.accept().await.is_err() {
                break;
            }
        }
    });

    let result = wrapper.dial_tcp(&loopback_metadata(addr.port())).await;
    accept_handle.abort();
    assert!(
        result.is_ok(),
        "候选[0] dial 挂起时应超时换到候选[1] 并成功，实际: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn test_dial_timeout_bounds_all_candidates_hang() {
    // 全链 + DIRECT 兜底都挂起（全黑洞）时，dial_tcp 必须在有界时间内
    // 返回 Err，而不是挂死请求。兜底超时 = max(dial, first_response)。
    let mut t = RuntimeTuning::from_eff(&default_eff());
    t.timeout_ms = 50; // dial 50ms，first_response 200ms → 兜底 200ms
    let tuning: SharedTuning = Arc::new(parking_lot::RwLock::new(t));

    let wrapper = ProxyWrapper::new(
        vec![
            Arc::new(HangingAdapter {
                health: ProxyHealth::new(),
            }) as Arc<dyn ProxyAdapter>,
            Arc::new(HangingAdapter {
                health: ProxyHealth::new(),
            }) as Arc<dyn ProxyAdapter>,
        ],
        Arc::new(HangingAdapter {
            health: ProxyHealth::new(),
        }),
        tuning,
        None,
    );

    // 500ms 足够覆盖 2×50ms 候选 + 200ms 兜底；修复前这里会挂死。
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        wrapper.dial_tcp(&loopback_metadata(443)),
    )
    .await;
    assert!(
        matches!(&result, Ok(Err(_))),
        "全黑洞时应在有界时间内返回 Err，实际超时或挂死"
    );
}

#[tokio::test]
async fn test_real_host_of_reverse_lookup() {
    // pure：store 里有映射的假地址 → 换回域名；没有映射的（真实 IP 或
    // 未命中）→ None（DIRECT 兜底原样拨，不误伤真直连流量）。
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new(64));
    let fake_ip: IpAddr = "198.18.0.5".parse().unwrap();
    store.put("www.gstatic.com", fake_ip);

    let wrapper = ProxyWrapper::new(
        vec![Arc::new(FailingAdapter {
            health: ProxyHealth::new(),
        })],
        Arc::new(DirectAdapter::new()),
        test_tuning(),
        Some(store),
    );

    let md = Metadata {
        dst_ip: Some(fake_ip),
        dst_port: 443,
        host: "www.gstatic.com".into(),
        ..Default::default()
    };
    assert_eq!(
        wrapper.real_host_of(&md).as_deref(),
        Some("www.gstatic.com"),
        "store 里的假地址应反查出域名"
    );

    let real = Metadata {
        dst_ip: Some("1.1.1.1".parse::<IpAddr>().unwrap()),
        dst_port: 443,
        ..Default::default()
    };
    assert_eq!(
        wrapper.real_host_of(&real),
        None,
        "真实 IP 不在 store 里，不应反查"
    );
}

#[tokio::test]
async fn test_init_rules_includes_private_network_excludes() {
    // 5 私网(硬编码) + 1 用户 CIDR + 1 FinalRule = 7 条，顺序固定。
    let rt = TunRuntime::new(
        empty_registry(),
        Arc::new(TokioRwLock::new(vec![])),
        test_tuning(),
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
        test_tuning(),
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
    reg.insert(
        "direct-a".into(),
        Arc::new(NamedStubAdapter {
            name: "direct-a",
            health: ProxyHealth::new(),
        }),
    );
    let registry: Registry = Arc::new(parking_lot::RwLock::new(reg));
    let selection: SharedSelection = Arc::new(TokioRwLock::new(vec!["direct-a".into()]));

    let rt = TunRuntime::new(
        registry,
        selection,
        test_tuning(),
        Some("198.18.0.0/15".into()),
        vec![],
    )
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
//   sudo cargo test --features meow-tun --test tun -- --ignored --test-threads=1

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
        let _ = run(
            config,
            empty_registry(),
            Arc::new(TokioRwLock::new(vec![])),
            test_tuning(),
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
            panic!("TUN 入口 run 在 1s 窗口内 panic: {join_err}");
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
        run(
            config,
            empty_registry(),
            Arc::new(TokioRwLock::new(vec![])),
            test_tuning(),
        ),
    )
    .await;
    assert!(res.is_ok(), "run 应在 3s 内返回，而非永久挂起");
}
