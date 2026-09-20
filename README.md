# silverq

稳定性优先的代理节点调度器（Rust）。

**只干两件事：测速 + 切换**——把池子里最稳定、最快的节点选出来，并切到它上。
协议/传输/TLS/Reality/QUIC **全部复用 meow-rs**，silverq 零自研协议。

## 工作原理

```
节点表 (nodes.yaml)
   │  NodeSpec: 协议 + 凭证
   ▼
factory::build_proxy ──▶ meow 协议 adapter (Vless/Trojan/SS/Hy2)   [复用 meow]
   │                        │
   │                        ▼
   │              MeowMeasurer: health::url_test (真实握手+探测)     [复用 meow]
   ▼
自适应 EWMA (方案 C: 波动 + 方向混合)  ← 分批并发测速, ranked 顺序
   │      超时/失败 → 大幅扣分 → 后移观察
   ▼
select_top(N)  纯 EWMA 取前 N（无迟滞轮次）
   │
   ▼
切换:  ① 写共享选择 (inbound 数据面读取, best→次优 fallback)
       ② 写 meow SelectorStore (外部 meow kernel 读取)
```

## 关键设计（按需求）

- **自适应 EWMA**：alpha 自动，无需手调。波动大→更稳，出现明确趋势→更快跟上。
- **分批并发**：节点池按当前排名分批测速，`buffer_unordered(concurrency)`，不阻塞在最慢节点。
- **快慢分离**：测速（周期全池离线）与切换（只读已算好的 EWMA）完全解耦，卡顿隔离。
- **超时扣分后移**：死/慢节点拿不到前排。
- **节点淘汰（pipeline 双指标 + 地板）**：连续失败 ≥ `retire_max_failures`（默认 5）触发检查——从未测通的节点立即摘除（从未通过真实握手不会自愈）；曾测通过的进入延长保活，连续失败连续满 `retire_keep_alive_secs`（默认 3600s）才摘。期间成功一次即复活。`retire_min_pool`（默认 10，0=禁用）兜底防摘到 0 全黑。可面板热改 + `silverq config-reload` 重读配置。
- **回环/私网/国内直连判定**（SOCKS 入站）：pin 优先，之后回环/私网目标原样直连、国内域名（`china-domains.txt`，11 万条后缀表）经真实 DNS 解析成 IP 后直连，其余走候选链。避免每个死候选烧 4s、国内绕远、私网目标被拨到节点侧 loopback。
- **域名级竞速（慢触发 + 滞回）**：某域名代理首响应 > 2s 才标记 slow，下次访问并行竞速（直连 + 代理候选，TCP 先建连者胜）；接管需滞回（新路线快过旧路线一半），TTL 5 分钟。无脑全域竞速会给每个域名加探测成本，故只对慢域名启用。已知盲区：纯 TCP 竞速分不出「TCP 通、TLS 断」——直连 relay 失败计 `direct_fails`，连续 2 次剔除回代理。TUN 路径未接入（首响应信号在 meow 引擎内部）。
- **节点 dial 真实 DNS 预解析**：节点服务器域名经固定上游（223.5.5.5 / 119.29.29.29，`SILVERQ_RESOLVE_UPSTREAMS` 可覆盖）预解析写 `NodeSpec.dial_addr`，adapter 拨号用真实 IP。修复 TUN + fake-IP 环境下「拨节点变成经候选链拨节点」的自指递归（2026-09-19 事故：健康检查幸存 14-30 → 0-1）。解析失败回退系统解析，只降级不丢节点。

## 使用

```bash
# 启动 daemon（调度 + 数据面 + ctl socket）
cargo run --features meow -- serve nodes.yaml

# 控制命令（另开终端，走 unix socket，不重启进程）
silverq status                # 当前池大小 / 选择 / 是否钉住
silverq reload [nodes.yaml]   # 热加载节点表：新增进池、删除出池、存活继承 EWMA
silverq select <tag>          # 手动钉住某节点（调度暂停覆盖）
silverq select auto           # 取消钉住，恢复自动
```

环境变量：

| 变量 | 默认 | 作用 |
|------|------|------|
| `SILVERQ_LISTEN` | `127.0.0.1:17321` | 数据面端口（SOCKS5 / HTTP-CONNECT） |
| `SILVERQ_CTL_SOCK` | `~/.local/state/silverq/ctl.sock` | 控制通道 socket |
| `SILVERQ_SELECTOR_STORE` | `~/.local/state/silverq-selector.json` | 外部 meow kernel 读的 selector store |
| `SILVERQ_STATE` | `~/.local/state/silverq/scores.json` | EWMA 分数存档 |
| `SILVERQ_RESOLVE_UPSTREAMS` | `223.5.5.5,119.29.29.29` | 节点 dial / 国内直连解析用的真实上游 DNS（逗号分隔） |
| `SILVERQ_CHINA_DOMAINS` | `~/.config/silverq/rules/china-domains.txt` | 国内域名后缀表（缺失 = 空表 = 无直连判定） |

无 `meow` feature 时用 `NoopMeasurer` 空跑（自测调度逻辑）：`cargo run -- serve nodes.yaml`

节点表见 `nodes.example.yaml`（VLESS / Trojan / Shadowsocks / Hysteria2 四协议示例）。

## 模块

```
src/
├── main.rs            # 进程装配：serve + 调度主循环
├── lib.rs             # 模块树（供集成测试与二进制复用）
├── config/            # settings.rs（silverq.toml）+ 默认参数/env 覆盖
├── scheduler/         # node.rs（Node+EWMA）、batch.rs（分批测速）、
│                      # decision.rs（select_top）、fast_path.rs（即时发布）、
│                      # persist.rs（EWMA 存档）；mod.rs（调度进度计数）
├── proxy/             # nodespec.rs（节点 YAML 模型）、factory.rs（NodeSpec→meow
│                      # adapter）、meow.rs（MeowMeasurer）、dns.rs（零依赖 UDP DNS
│                      # 客户端 + 国内域名表 ChinaSet）、route.rs（域名级竞速缓存）
│                      # ——meow feature
├── dataplane/         # inbound.rs（SOCKS5/HTTP-CONNECT TCP）、udp.rs（UDP 中继）、
│                      # tun.rs（TUN 透明代理，meow-tun feature）——meow feature
├── web/               # mod.rs（内嵌面板 + JSON API）——meow feature
└── ctl/               # cli.rs（子命令解析）、protocol.rs（unix socket 控制通道）
```

## 已验证 / 已知范围

### 自动化测试（106 项，`cargo test --features meow`）

41 lib 单测 + 65 集成测试。测试按源文件划分放在 `tests/`（`decision.rs`/`inbound.rs`/… 与 `src/`
模块一一对应）。依赖 meow 的测试文件带 `#![cfg(feature = "meow")]`，纯 `cargo test`
也能跑非协议部分。e2e 真起 `silverq serve` 进程、用真 SOCKS5 / HTTP-CONNECT 客户端打流量，
目标是本地 echo 服务、探测端点也在本地 —— **全程回环，不依赖外网**，CI 可稳定跑。

| 覆盖 | 内容 |
|------|------|
| SOCKS5 TCP CONNECT | 握手协商 + 完整 10 字节应答 + echo 往返 |
| SOCKS5 UDP ASSOCIATE | 中继端口分配 + 头部编解码 + UDP echo 往返 + 回程来源正确 |
| UDP 分片 | `FRAG != 0` 丢弃（不做重组） |
| SOCKS5 BIND | 回 `0x07` command not supported |
| HTTP CONNECT | 200 应答 + 隧道无头部残留（曾污染 TLS ClientHello 的回归点） |
| Reality 公钥 | 43 字符无 padding base64url（真实形态）+ 带 padding 变体 |

### 真实节点池实测（本地，不进 CI）

节点池从旧栈的 `nodes.json` 一次性迁移转出 **217 个节点**
（vless 161 / hysteria2 23 / shadowsocks 22 / trojan 11；迁移脚本已完成使命删除），实测结果：

- 217/217 全部成功构建 meow adapter（四种协议 + ws/grpc transport 都真实跑过）
- 经 silverq + 真实代理节点出网：`http_code=204`，稳定 0.18~0.22s
- 出口 IP 确认为代理落地 IP（直连被墙 → 证明没有静默走直连兜底）
- UDP ASSOCIATE 经真实节点查 DNS：回包 tid 匹配、`ANCOUNT=2`

对照旧栈同节点测速可知：池中大量节点（含 46 个 Vision+Reality）**本身已死** ——
旧栈测同样超时。这不是 silverq 的问题，排查时容易误判成协议 bug。

### Web 面板

双面板,同一端口:

**`/ui/` — zashboard**(外部 dashboard,MIT,`scripts/fetch-zashboard.sh` 下载)
通过 clash 兼容 API 对接:节点列表+延迟历史图、组内切换(= 钉住)、
配置面板(PATCH /configs 热生效,不写回 TOML)、连接/日志页。
zashboard 首次打开在 setup 页填 `127.0.0.1` + `9095`,或直接访问
`/ui/?hostname=127.0.0.1&port=9095` 自动配置。

**`/` — 内嵌轻量面板**:silverq 特有数据(纯实测延迟 + EWMA 历史曲线、
连续失败次数、可用/不可用/待测三态、健康度汇总、调度轮/批进度),
3 秒轮询,无前端依赖。样式与图表原语来自 [uikit](../uikit) 模板
(vendored 于 `src/web/uikit/`,头部注释标了来源 commit)。

| 端点 | 说明 |
| --- | --- |
| `GET /` | 内嵌轻量面板 |
| `GET /ui/` | zashboard(需先 fetch-zashboard.sh) |
| `GET /api/status` | JSON:节点表、EWMA、样本、selection、pinned |
| `GET /version` `/proxies` `/configs` `/providers/*` `/connections` `/rules` | clash 兼容 API |
| `GET /traffic` `/memory` `/logs` `/connections`(WebSocket) | clash 兼容 WS(数据恒 0) |
| `PUT /proxies/{group}` | 切换/钉住(name=auto 解钉) |
| `PATCH /configs` | 热更调度参数(capacity/batch_size/interval_secs/timeout_ms/concurrency/timeout_penalty/fallback_attempts) |
| `GET /api/health` | 存活探针 |

**无认证**,只绑回环;不要改成 `0.0.0.0`。改端口:

```toml
[data_plane]
listen = "127.0.0.1:17321"
web_listen = "127.0.0.1:9095"
ui_dir = "~/.local/share/silverq/ui"   # zashboard 静态目录
```

## 已知范围

- **TUN 已实现（`meow-tun` feature）**：基于 meow-listener 的 listener-tun。
  fake-IP 路由（`TunRouteScope::FakeIp` 只接管 fake-IP 段，真实 IP 不回环，
  故节点服务器 IP 不会绕回 TUN；若观察到异常，在 `[tun].exclude_cidrs` 加
  `节点IP/32`），规则分流（私网 + 用户 `exclude_cidrs` → DIRECT，末尾
  FinalRule → silverq-auto）。出口候选链每个 dial 包超时（与测速超时等值，
  黑洞节点 SYN 丢弃时不会挂死请求——2026-09-20 修复，修复前级联停顿可达
  一个健康检查周期），候选数截断到 `fallback_attempts`，全部失败后反查
  fake-IP → 真实 DNS 解真身走 DIRECT（解析结果过私网/回环判定，失败关闭）。
  建设备需要 root 或 `CAP_NET_ADMIN`；只有 release 里的 `silverq-tun-*`
  产物带这个 feature。**当前部署未启用**（`[tun].enabled = false`）。
- **ws / grpc 已支持**（VLESS）：层序为 TLS 贴 TCP、ws/grpc 叠其上，明文 ws 节点
  （`tls: false`）也可接。trojan 的 transport 还没接（其 adapter 无 TransportChain 入口）。
- **黑洞节点已处理**：建连后加了「首次响应超时」（测速超时 ×4）。顺序是先把客户端
  第一批数据转发过去、再等对端回应——不能盲等首字节，因为多数协议是客户端先说话
  （TLS ClientHello / HTTP 请求），盲等会把正常连接全判死。超时只作用于首次响应，
  之后进入无超时拷贝，避免误杀长连接。实测钉住黑洞节点从卡满 25s 降到 ~6s 失败。
- **EWMA 已持久化**：每轮结束原子写盘（`SILVERQ_STATE`），重启自动恢复。
  存档超过 6 小时视为陈旧、一律丢弃——几小时前的延迟不能拿来做当下决策。
  只存 `ewma` 与 `samples`，不存 `recent` 窗口（它只影响自适应 alpha 的头几次取值）。
- **trojan 的 transport 无法接**：meow 的 `TrojanAdapter` 只持有 `Arc<TlsLayer>`，
  `dial_tcp` 里硬编码 `TCP → tls_layer.connect → 写 header`，公开构造函数只有
  `new` 和 `with_mux`，**没有插入 ws/grpc 层的位置**。要支持需上游给 trojan 加
  TransportChain 入口。真实池里因此跳过 4 个 trojan+ws 节点。
- **UDP 不做分片重组**。
- SOCKS5 inbound **无认证**，默认只绑 `127.0.0.1`。改绑 `0.0.0.0` 等于开放代理。
- 冷启动：真实池 217 节点跑完一轮约 90s。已用"配置顺序播种 + 每批发布中间结果 +
  **交错分批**（已测与未测混测，见 `measurement_order`）"把活节点发现时间从
  "整轮跑完才出现"降到 45s 内。头几秒仍可能选到死节点（靠 fallback 兜）。

## CI

`.github/workflows/ci.yml`：

| job | 作用 |
|-----|------|
| `rustfmt` | 格式 gate |
| `clippy (default / meow / meow-tun)` | 三种 feature 组合，`-D warnings` |
| `test (default / meow / meow-tun)` | build + 单测；meow / meow-tun 额外跑 e2e 数据面 |
| `TUN placeholder not wired` | 守住未开 `meow-tun` 时 `tun::run` 不被无 cfg 保护的调用点引用 |

三种 feature 都进矩阵的原因：meow 关掉时走 `NoopMeasurer`，是独立编译路径；
`meow-tun` 多编译一整块 TUN 数据面，只测两种会漏掉 cfg 分支里的错误（已踩过）。

## TUN 手动测试

### 直连 fallback（防断网）

v0.2.0 给 TUN 数据面加了两层 DIRECT 兜底，避免节点挂掉时连本地网关 / DNS 都打不通：

1. **私网 + 用户排除 CIDR 自动走 DIRECT** —— 规则表首部固定注入 5 段私网
   （`10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`、`127.0.0.0/8`、`169.254.0.0/16`），
   再叠加配置里 `[tun].exclude_cidrs` 的条目，全部 `→ DIRECT`，先于末尾的
   `silverq-auto` FinalRule 命中。非法 CIDR 逐条 `warn` 跳过，不阻断启动。
2. **节点 dial 失败自动回退 DIRECT** —— `silverq-auto` 包装的主出口若 `dial_tcp` /
   `dial_udp` 出错，自动改走共享的 DIRECT 适配器，而不是把错误透传给上层连接。

Private networks and user `[tun].exclude_cidrs` always route to DIRECT; when the
selected node fails to dial, `silverq-auto` transparently falls back to DIRECT
instead of propagating the error.

TUN 设备创建需要 root 或 `CAP_NET_ADMIN`，所以这类 smoke 测试标了 `#[ignore]`、
**不进 CI**（runner 无 root）。CI 只覆盖 `src/tun.rs` 里的纯逻辑单测：
`TunConfig::from_effective` 字段映射、`ProxyWrapper` 的 Proxy 桩与 `ProxyAdapter` 委托、
`run` 拒绝非法 `fake_ip_cidr`，以及 `init_rules` / `sync_proxies`（经
`meow_tunnel::Tunnel::route_snapshot()` 公开 getter 验证，无需真建设备）。

本地手动跑（silverq 是 bin-only crate，这些 smoke 测试放在 `src/tun.rs` 的
`#[cfg(test)]` 模块内，而非 `tests/` 集成测试 —— 后者拿不到 `tun::run`）：

```bash
sudo cargo test --features meow-tun --bin silverq -- --ignored --test-threads=1
```

会真的尝试创建 TUN 设备（`auto_route=false`，不动宿主路由表）。有 root 才能真正建出
设备并阻塞运行；无 root 时 `run` 会快速返回错误而非挂死 —— 这两种情况 smoke 都算过。

## License

MIT
