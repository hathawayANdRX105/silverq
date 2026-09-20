# silverq 工作约定

> 本文件写**每个会话都必须遵守的硬约束**，和**遇到什么情况该读哪份文档**。
> 与全局 `~/.omp/agent/AGENTS.md` 互补：那里管通用行为，这里管 silverq 特有红线。
> 详细操作说明不放在本文件，按文末导航表去读。

---

## 本地重命令禁令（最高优先级）

**宿主机不跑 `cargo test` / `cargo clippy` / 多 feature 编译矩阵。** 全部验证推 PR 走 CI。
本机 8 核 i5 常驻 docker + chrome + electron，本地跑重命令会拖垮用户环境并影响生产
silverq 实例——用户曾因此严厉警告（2026-09-20）。

允许的本地命令：

| 命令 | 条件 |
|---|---|
| `cargo check --features meow-tun --all-targets` | 增量检查，推送前确认能编译（秒级） |
| `cargo fmt --all` | 随时 |
| `cargo build --release --features meow-tun` | **仅部署需要产物时**，且必须 `cpulimit -l 65 -i --` 包裹 |
| `~/.local/bin/silverq-lab.sh` | 容器实验室 gauntlet，数据面/TUN 实验的唯一本地运行场所 |
| `git` / `gh` / `grep` / 文件读写 | 轻量，不限 |

CPU 密集型命令（编译/测试/装包）一律 `cpulimit -l 65 -i --` 前缀，禁止裸跑。

## 宿主机生产实例保护（硬约束）

生产 silverq 由**用户自有 nohup** 跑（`~/.local/bin/silverq serve ~/.config/silverq/nodes.yaml
--config ~/.config/silverq/silverq.toml`，日志 `~/.local/state/silverq/serve.log`），
`poll-jobs`（`~/.local/bin/poll/silverq-check`，120s 探活）负责守护。

- **禁止随手 `pkill -f silverq`**：会杀掉临时调试实例以外的东西，且 poll 会在 120s 内
  抢拉起一个**不持 17321 但顶掉 ctl.sock** 的救援实例（2026-09-20 部署时踩过：
  两个进程一个持端口一个持 socket）。停生产实例前想清楚，停后一次起干净。
- **禁止在 hub 里 start silverq**：会和用户实例抢 17321。
- **部署流程**：`cargo build`（cpulimit）→ 停旧 PID（按 PID，不宽匹配）→ 换二进制 →
  nohup 起 → gauntlet（baidu/gstatic/github 走 `socks5h://127.0.0.1:17321`）+
  ctl status 验证。换二进制窗口期 poll 可能双杀，收尾时确认只有一个实例。
- **回滚**：`~/.local/bin/silverq-rollback.sh`（恢复 `silverq.good` 并重启，不依赖网络）。
  新版本部署验证通过后，才把 `silverq.good` 更新为新版（`cp $BIN $GOOD`）。
- **TUN 搁置**：用户 2026-09-20 明确决定 TUN 不启用、不继续做（节点不稳时故障面大于
  收益）。不要重新提议启用。`[tun].enabled = false` 是终态；TUN 代码（含 dial 超时修复）
  保留在 master，不删不改。

## 删除与清理（硬约束）

- 本地非 Git 跟踪文件：一律 `gio trash <path>`（回收站可恢复）。**严禁 `rm`/`rm -rf`/
  `rmdir`/`unlink`/`gio remove`**（2026-09-20 曾对 `/tmp` 临时目录误用 `rm -rf`，临时目录
 也不行，规矩就是规矩）。清空回收站须用户明确指令。
- Git 跟踪文件：`git rm`。严禁任何形式 `git clean`（会擦掉未跟踪的本地配置和跨会话产物）。
- 只能删自己负责的已合并分支/worktree；删前确认 PR 已合。

## 密钥与敏感信息

- `nodes.yaml` 含真实节点凭证，只在 `~/.config/silverq/`，**永不入库**（仓库只有
  `nodes.example.yaml` 占位）。提交前扫一眼 diff。
- 文档/示例里的地址用 `127.0.0.1` 或占位符。

## PR 与合并

- 合并方式：**squash merge**（用户 2026-09-20 指定，减少 commit 冗余）。
  命令必须带 `--body "Agent 🤖 - Merge: <原因>"`（仓库闸门强制）。
- 合并前 PR 正文 checklist 必须全勾（含 N/A 项要注明理由）——闸门会逐项检查。
- 拦截信息逐条读完再修根因；禁止 `--no-verify`、禁止 `| head` 截断后忽略。FAIL 必须清零。
- stacked PR 按依赖序合并（head 分支先合，base 后合）；base 分支更新后等 CI 重跑绿了再合下一层。

## 测试约定

- 测试按源文件划分放 `tests/`（`decision.rs` ↔ `src/scheduler/decision.rs`）。
  silverq 是 bin-only crate，`src/dataplane/tun.rs` 的测试在其模块内 `#[cfg(test)]`。
- e2e（`tests/e2e_data_plane.rs`）真起进程、真客户端，但**全程回环**，不依赖外网。
- 新测试必须防一个合理会发生的 bug；不为"有测试而写测试"；禁止断言实现细节
  （字段拷贝、默认值、mock 回声）。
- 「通过」= CI 全绿（三种 feature 矩阵 + clippy + fmt + e2e）。本地不判通过。

## 遇到什么情况，读哪份文档

| 场景 | 文档 |
|---|---|
| 发新版本（版本号怎么算、tag、Release 核对） | `.agent/tasks/versioning.md`（流程）+ 根 `VERSIONING.md`（功能域清单真相源） |
| TUN 历史排查结论、实验室用法、部署拓扑 | `todo/tun-handover.md`（已冻结，TUN 不做了，仅作史料） |
| 数据面/TUN 实验（不动宿主机） | `~/.local/bin/silverq-lab.sh`（容器 netns 完整透明链路） |
| 提交/推送/合并被拦 | `.githooks/GATE_HANDBOOK.md` + `.githooks/spec/SPEC_OVERVIEW.md` |
| CI 结构与三种 feature 矩阵的理由 | 根 `README.md` 的 CI 一节 |
| 调度/数据面行为细节 | 根 `README.md`（关键设计 + 已知范围，与代码同步维护） |

## 环境事实

- 机器 8 核 i5-9300H，docker + chrome + electron 常驻，load 容易上 15——复现问题注意
  排除负载变量，重命令想清楚再跑（见顶部禁令）。
- 免费节点池成批死亡是上游供给问题，不是 silverq 的 bug；github/gstatic 时快时慢属此类。
- `git push` 直连可用（`git -c http.proxy= push`）；走 silverq 代理反而不稳。
  `gh` 命令加 `env -u HTTPS_PROXY -u HTTP_PROXY -u https_proxy -u http_proxy` 走直连。
