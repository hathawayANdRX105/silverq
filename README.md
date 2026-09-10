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
- **切换=selector**：silverq 自己就是 selector；meow 只负责连接。

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

无 `meow` feature 时用 `NoopMeasurer` 空跑（自测调度逻辑）：`cargo run -- serve nodes.yaml`

节点表见 `nodes.example.yaml`（VLESS / Trojan / Shadowsocks / Hysteria2 四协议示例）。

## 模块

| 文件 | 职责 |
|------|------|
| `node.rs` | Node + 自适应 EWMA（方案 C）+ adopt_score（reload 继承） |
| `nodespec.rs` | 节点 YAML 配置模型（协议 + 凭证 + 校验） |
| `factory.rs` | NodeSpec → meow 协议 adapter |
| `meow.rs` | MeowMeasurer（按 tag 查 adapter，委托 meow `health::url_test`） |
| `batch.rs` | 分批并发测速 + Measurer trait |
| `fast_path.rs` | 立即应用结果 + 超时扣分 |
| `decision.rs` | 纯 EWMA select_top |
| `inbound.rs` | 数据面 TCP（SOCKS5 CONNECT / HTTP-CONNECT → 当前选择，best→次优 fallback） |
| `udp.rs` | 数据面 UDP（SOCKS5 UDP ASSOCIATE 中继，按 (客户端,目标) 分会话） |
| `tun.rs` | **未实现占位**（`unimplemented!`），见文件内说明 |
| `cli.rs` | 子命令解析（serve / reload / select / status） |
| `ctl.rs` | 控制通道（unix socket：热加载、手动钉住、状态查询） |
| `persist.rs` | EWMA 分数持久化（原子写 + 6 小时过期判定） |
| `config.rs` | 默认参数 + 环境变量覆盖（探测 URL / 间隔 / 超时 / 容量） |
| `scripts/singbox2silverq.py` | sing-box `nodes.json` → silverq YAML（凭证只在本地文件间流动） |

## 已验证 / 已知范围

### 自动化测试（22 项，`cargo test --features meow`）

17 单测 + 5 e2e。e2e 真起 `silverq serve` 进程、用真 SOCKS5 / HTTP-CONNECT 客户端打流量，
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

用 `scripts/singbox2silverq.py` 从 sing-box 的 `nodes.json` 转出 **217 个节点**
（vless 161 / hysteria2 23 / shadowsocks 22 / trojan 11），实测结果：

- 217/217 全部成功构建 meow adapter（四种协议 + ws/grpc transport 都真实跑过）
- 经 silverq + 真实代理节点出网：`http_code=204`，稳定 0.18~0.22s
- 出口 IP 确认为代理落地 IP（直连被墙 → 证明没有静默走直连兜底）
- UDP ASSOCIATE 经真实节点查 DNS：回包 tid 匹配、`ANCOUNT=2`

对照 sing-box 同节点测速可知：池中大量节点（含 46 个 Vision+Reality）**本身已死** ——
sing-box 测同样超时。这不是 silverq 的问题，排查时容易误判成协议 bug。

### Web 面板

双面板,同一端口:

**`/ui/` — zashboard**(外部 dashboard,MIT,`scripts/fetch-zashboard.sh` 下载)
通过 clash 兼容 API 对接:节点列表+延迟历史图、组内切换(= 钉住)、
配置面板(PATCH /configs 热生效,不写回 TOML)、连接/日志页。
zashboard 首次打开在 setup 页填 `127.0.0.1` + `9095`,或直接访问
`/ui/?hostname=127.0.0.1&port=9095` 自动配置。

**`/` — 内嵌轻量面板**:silverq 特有数据(纯实测延迟、连续失败次数、
可用/不可用/待测三态、健康度汇总),3 秒轮询,无前端依赖。

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

- **TUN 未实现**：`src/tun.rs` 是显式占位（`unimplemented!` / `todo!`），不接线。
  meow-rs 上游有 TUN 但**未发布到 crates.io**；要做需改 git 依赖复用上游，
  或自行基于 `tun` + `smoltcp` 实现。两条路线与证据见该文件模块文档，CI 有 job 守着它没被误接。
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
- EWMA 分数**进程重启后清零**（`reload` 不丢，只有重启丢）。
- 冷启动：真实池 217 节点跑完一轮约 90s。已用"配置顺序播种 + 每批发布中间结果 +
  **交错分批**（已测与未测混测，见 `measurement_order`）"把活节点发现时间从
  "整轮跑完才出现"降到 45s 内。头几秒仍可能选到死节点（靠 fallback 兜）。

## CI

`.github/workflows/ci.yml`：

| job | 作用 |
|-----|------|
| `rustfmt` | 格式 gate |
| `clippy (default / meow)` | 两种 feature 组合，`-D warnings` |
| `test (default / meow)` | build + 单测；meow 额外跑 e2e 数据面 |
| `TUN placeholder not wired` | 防止未实现的 TUN 占位被误接进运行路径 |

两种 feature 都进矩阵的原因：meow 关掉时走 `NoopMeasurer`，是独立编译路径，
只测一种会漏掉 `cfg` 分支里的错误。

## License

MIT
