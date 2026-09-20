# 版本口径（silverq）

三段各来源不同：

- **major = 用户确认**（breaking 由用户拍板，当前 0）
- **minor = 用户可感知功能域数**（逐模块代码清点，排除管道模块，可复现）
- **patch = fix 类型 commit 累计数**（基于发版分支 master：`git log master --no-merges --format="%s" | grep -cE "^fix"`）

功能域判定：该能力若被移除，用户/前端是否察觉？察觉=计入，不察觉（内部存储/解析/解码/模型/组装）=管道排除。

## 功能域清单（minor = 12，能力域粒度）

| 功能域 | 位置 |
|---|---|
| SOCKS5/HTTP 入站数据面 | src/dataplane/inbound.rs |
| SOCKS5 UDP ASSOCIATE 中继 | src/dataplane/udp.rs |
| 批量并发节点测量 | src/scheduler/batch.rs |
| EWMA 选型决策 | src/scheduler/decision.rs |
| 快速路径（实测结果即时应用） | src/scheduler/fast_path.rs |
| EWMA 分数持久化（重启保留） | src/scheduler/persist.rs |
| 节点配置模型 / YAML 节点表 | src/proxy/nodespec.rs |
| MeowMeasurer 节点测量 | src/proxy/meow.rs |
| CLI 子命令 | src/ctl/cli.rs |
| Unix socket 控制通道 | src/ctl/protocol.rs |
| Web 面板 | src/web/ |
| serve 数据面组装 / 启动通路 | src/main.rs |

排除（管道）：`config`（配置解析）、`proxy/factory`（NodeSpec→proxy 装配）、
`dataplane/tun`（用户主动不用，移除不感知——见快照备注）、`scheduler/node`（节点内部模型）。

## 快照（v0.12.24）

- major = 0（用户确认，无 breaking）
- minor = 12（上表 12 项功能域）
- patch = 24（master 全历史 fix 提交累计）

> v0.12.24 备注：`dataplane/tun` 排除理由「未实现占位」已失效（TUN 于 #4 实现并在
> master，用户主动关闭不用）。本次按「移除后用户是否察觉 → 用户不用即不察觉」仍计管道，
> minor 维持 12；下次发版如用户重新启用 TUN 需重判。

重新清点：新增功能域加一行 +1；移除 -1；patch 重跑命令。改 Cargo.toml `version` + 本文件快照。
