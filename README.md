# lift

稳定性优先的代理节点调度器（Rust）。

**只干两件事：测速 + 切换**——把池子里最稳定、最快的节点选出来，并切到它上。
协议/传输/TLS/Reality/QUIC **全部复用 meow-rs**，lift 零自研协议。

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
- **切换=selector**：lift 自己就是 selector；meow 只负责连接。

## 使用

```bash
# 启动 daemon（调度 + 数据面 + ctl socket）
cargo run --features meow -- serve nodes.yaml

# 控制命令（另开终端，走 unix socket，不重启进程）
lift status                # 当前池大小 / 选择 / 是否钉住
lift reload [nodes.yaml]   # 热加载节点表：新增进池、删除出池、存活继承 EWMA
lift select <tag>          # 手动钉住某节点（调度暂停覆盖）
lift select auto           # 取消钉住，恢复自动
```

环境变量：

| 变量 | 默认 | 作用 |
|------|------|------|
| `LIFT_LISTEN` | `127.0.0.1:17321` | 数据面端口（SOCKS5 / HTTP-CONNECT） |
| `LIFT_CTL_SOCK` | `~/.local/state/lift/ctl.sock` | 控制通道 socket |
| `LIFT_SELECTOR_STORE` | `~/.local/state/lift-selector.json` | 外部 meow kernel 读的 selector store |

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
| `inbound.rs` | 数据面（SOCKS5/HTTP-CONNECT → 当前选择，best→次优 fallback） |
| `cli.rs` | 子命令解析（serve / reload / select / status） |
| `ctl.rs` | 控制通道（unix socket：热加载、手动钉住、状态查询） |
| `config.rs` | 默认参数 |

## 已知范围（MVP）

- 数据面为 **SOCKS5 / HTTP-CONNECT**，UDP 数据面与 TUN 未做（测速走 TCP 探测）。
- EWMA 分数**未持久化**，重启后从零累积。
- selector 切换依赖外部 meow kernel 轮询 `SelectorStore`，lift 自身不驱动内核热重载。

## License

MIT
