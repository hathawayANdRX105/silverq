<!-- managed by canon agents.yaml @ 2026-09-24 -->
## silverq 约定

> 本文件写**每个会话都必须遵守的硬约束**，和**遇到什么情况该读哪份文档**。
> 与全局 `~/.omp/agent/AGENTS.md` 互补：那里管通用行为，这里管 silverq 特有红线。
> 详细操作说明不放在本文件，按文末导航表去读。

---

### 本地重命令禁令（最高优先级）

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

### 宿主机生产实例保护（硬约束）

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

### 删除与清理（硬约束）

- 本地非 Git 跟踪文件：一律 `gio trash <path>`（回收站可恢复）。**严禁 `rm`/`rm -rf`/
  `rmdir`/`unlink`/`gio remove`**（2026-09-20 曾对 `/tmp` 临时目录误用 `rm -rf`，临时目录
 也不行，规矩就是规矩）。清空回收站须用户明确指令。
- Git 跟踪文件：`git rm`。严禁任何形式 `git clean`（会擦掉未跟踪的本地配置和跨会话产物）。
- 只能删自己负责的已合并分支/worktree；删前确认 PR 已合。

### 密钥与敏感信息

- `nodes.yaml` 含真实节点凭证，只在 `~/.config/silverq/`，**永不入库**（仓库只有
  `nodes.example.yaml` 占位）。提交前扫一眼 diff。
- 文档/示例里的地址用 `127.0.0.1` 或占位符。

### PR 与合并

- 合并方式：**squash merge**（用户 2026-09-20 指定，减少 commit 冗余）。
  命令必须带 `--body "Agent 🤖 - Merge: <原因>"`（仓库闸门强制）。
- 合并前 PR 正文 checklist 必须全勾（含 N/A 项要注明理由）——闸门会逐项检查。
- 拦截信息逐条读完再修根因；禁止 `--no-verify`、禁止 `| head` 截断后忽略。FAIL 必须清零。
- stacked PR 按依赖序合并（head 分支先合，base 后合）；base 分支更新后等 CI 重跑绿了再合下一层。

### 测试约定

- 测试按源文件划分放 `tests/`（`decision.rs` ↔ `src/scheduler/decision.rs`）。
  silverq 是 bin-only crate，`src/dataplane/tun.rs` 的测试在其模块内 `#[cfg(test)]`。
- e2e（`tests/e2e_data_plane.rs`）真起进程、真客户端，但**全程回环**，不依赖外网。
- 新测试必须防一个合理会发生的 bug；不为"有测试而写测试"；禁止断言实现细节
  （字段拷贝、默认值、mock 回声）。
- 「通过」= CI 全绿（三种 feature 矩阵 + clippy + fmt + e2e）。本地不判通过。

### 遇到什么情况，读哪份文档

| 场景 | 文档 |
|---|---|
| 发新版本（版本号怎么算、tag、Release 核对） | `.agent/tasks/versioning.md`（流程 + 功能域清单） |
| TUN 历史排查结论、实验室用法、部署拓扑 | `todo/tun-handover.md`（已冻结，TUN 不做了，仅作史料） |
| 数据面/TUN 实验（不动宿主机） | `~/.local/bin/silverq-lab.sh`（容器 netns 完整透明链路） |
| 提交/推送/合并被拦 | `.githooks/GATE_HANDBOOK.md` + `.githooks/spec/SPEC_OVERVIEW.md` |
| CI 结构与三种 feature 矩阵的理由 | 根 `README.md` 的 CI 一节 |
| 调度/数据面行为细节 | 根 `README.md`（关键设计 + 已知范围，与代码同步维护） |

### 环境事实

- 机器 8 核 i5-9300H，docker + chrome + electron 常驻，load 容易上 15——复现问题注意
  排除负载变量，重命令想清楚再跑（见顶部禁令）。
- 免费节点池成批死亡是上游供给问题，不是 silverq 的 bug；github/gstatic 时快时慢属此类。
- `git push` 直连可用（`git -c http.proxy= push`）；走 silverq 代理反而不稳。
  `gh` 命令加 `env -u HTTPS_PROXY -u HTTP_PROXY -u https_proxy -u http_proxy` 走直连。

## 发现处置纪律

自动检查（gate 的 `FAIL`/`WARN`、`jev` L3 语义发现、CRG / `ocr review` 审查意见）
产出的是**发现**，不是判决。每条发现都必须被显式处置，不存在"绕过"这个选项。

### 先读规范，再改代码

1. 拿到 finding，先读规则原文，确认这条发现到底要求什么：
   - gate 规则总览：`.githooks/GATE_HANDBOOK.md`（无则 `canon/manual/gate.md`）
   - 单条规则的参数（匹配范围 / 严重度 / harness）：`.githooks/spec/**/<rule>.yaml`
   - 项目适配说明（本仓为什么这么定）：`.agent/rules/gates.md`
2. 不确定 finding 是否成立时，读完规则仍不能判定 → **记为待裁决**并在交付记录里写明，
   不要凭猜测改代码，也不要直接忽略。

### 按根因修，不按症状修

- finding 指向的**约束**是根因。修代码使约束成立，而不是让检查不再报。
- 修完自问：这条约束在本仓还成立吗？下次同类改动还会不会触发？

### 完整读输出，不截断

- 拦截信息**逐条读完**再动手。`| head -5`、`| tail`、`grep -v` 会吞掉后面的 finding，
  让人误以为已经修完。
- 报告里出现「N checks passed」时，确认 N 覆盖了你改动的部分。

### 禁止糊弄式修复

以下动作一律视为违规（无论 gate 是否因此变绿）：

| 禁止 | 为什么 | 正确做法 |
|---|---|---|
| 改 `.githooks/spec/` 规则、降低 `fail_severity`、删 spec 文件 | 把约束改没，不是修问题 | 开 issue 说明规则缺陷，交维护者决定 |
| `--no-verify`、跳过钩子、直接推 | 绕过的是整个门禁体系 | 修到清零；规则有误走 issue |
| `head` / `tail` / `grep -v` 截断输出后当没看见 | 后面的 finding 被吞 | 完整读输出 |
| 加 `#[allow(dead_code)]` / `# noqa` 消告警 | 压制信号而非解决 | 删无用代码，或写清保留理由 |
| 建空文件 / 空目录 / 占位文件骗过目录类规则 | 结构噪音 | 真按规则合并或删除 |
| 给无断言测试塞 `assert!(true)` | 测试变成永真装饰 | 断言真实行为；无行为可测就删测试 |
| 拆分 / 改名 / 移动只为躲过匹配范围 | 破坏结构换绿灯 | 按规则设计的结构改 |

### 逐条处置并留下书面说明

- **每条 finding 一个处置**：修复（默认）或**书面驳回**。
- 修复 → 在交付记录里写：`规则 ID → 根因 → 改法（file:line）`。
- 驳回 → 必须写 `规则 ID + 不修理由 + 依据`，由维护者裁决。沉默即违规。
- 交付记录落点：PR 正文 `## Delivery record` 段，或 issue 的交付评论。
- WARN 与 FAIL 同等对待。WARN 只是不拦，不是可忽略。

### 规范层级

- `.githooks/` 是 gate 领地：agent 不改规则。
- `.agent/rules/`、`specs/rules/` 是规范正本：发现规则与现实冲突 → 提 issue，不自行改写。
- 本纪律与各仓既有条款冲突时，以本纪律为准（它更严格）。

## 代码风格

### 命名与结构

- 函数名动宾结构、见名知目的（`parse_channel_config` 而不是 `do_config`）。
- 公共 API 写文档注释（用途、参数、错误、示例），模块头写 `//!`。
- 变量与类型不缩写到看不出含义；短名只留给公认短物（`id`、`ctx`、`err`）。

### 注释

- 注释写**为什么**，不复述代码在做什么。
- 不留 AI 味注释（`// Step 1:` / `// This function` / `// 该函数…` / `// 首先…然后…`）。
- 需要解释的复杂逻辑，宁可提取成命名清晰的函数，也不要靠注释块描述流程。
- 注释掉的代码直接删；git 记得它。

### 占位符与未完成

- 未实现的函数或 trait 用语言原生宏，并带 issue 号：
  - Rust：`todo!("TODO(#123): 说明这里要做什么")` / `unimplemented!("…")`
- TODO / FIXME 注释必须带 issue 号：`// TODO(#123): …`。
- 不留空的 `todo!()` / `pass` / `NotImplemented` 桩而无说明。

### 复用与删除

- 动手前先找同仓同类实现与已装依赖。已有工具能解决就不新写。
- 新增依赖前确认：标准库能做完？已装依赖能做？确实都需要才加。
- **删除优于新增**：不留兼容垫片、旧别名、废弃分支、注释掉的旧实现。
- 改了接口就同步迁移所有调用方，不留双路径兼容。

### 工具

- 命名、缩进、格式化交给项目工具（`cargo fmt` / `gofmt` / `ruff format` / `prettier` / `biome`），
  不手工对齐，不在格式化工具之外争论风格。
- lint 报错逐条判断：真问题就修；误报就在规则允许的方式下局部豁免并写明理由，
  不整文件关掉。

## 破坏性操作与敏感信息

### 删除

- 删文件前确认它确实是废弃物（生成物、已合并的临时文件），不是"看起来没用"。
- 用可恢复的方式删（`gio trash`），不用不可恢复的直接删除。
- `rm -rf`、覆盖写、清空数据库这类不可逆操作：**先说明影响，等确认**。
- 删的是别人的产物、你不理解用途的文件、或 gitignore 里的东西 → 停下来问。

### 敏感与不可逆

- 凭据、token、密钥、私钥：不打印到输出、不写进提交、不粘到 issue/PR 正文。
- 不擅自 dump 整个配置文件或环境变量（可能含密钥）。要看就只看需要的字段。
- 系统级配置、字体、全局环境、dotfiles 里的全局项：默认别动，改动前先问。
- 数据库迁移、配置格式变更、依赖大版本升级：先确认可回滚。

### 安装与全局改动

- 装包、改 PATH、装 systemd 服务、改 shell 配置：先确认再动。
- 写进 dotbot / 配置管理器托管范围的路径前，先确认该由谁管。
- 不可逆的系统级改动（分区、引导、网络栈）一律先问，不自行执行。

## 提交与 PR

### 分支

- 默认分支是 `main`（本仓若不同以本仓为准），功能从默认分支拉。
- 一个任务一个分支，分支名带类型前缀（`feat/` / `fix/` / `refactor/` / `chore/`）。
- 合并后清理已合并分支与 worktree，不留 stale 分支。

### Commit

- 标题走 conventional commit（`feat:` / `fix:` / `refactor:` / `docs:` / `chore:` /
  `test:` / `ci:` / `build:` / `perf:` / `style:` / `revert:`）。
- 标题**用英文**，正文可用中文。
- 一个 commit 一件事。不把无关改动、格式化噪声、生成物混进逻辑改动。
- 提交前跑对应检查（`gate pre-commit` / `gate pre-push`），不靠推送失败才发现。

### Issue

- 标题中文；正文 heading 英文、内容中文。
- sub-issue 必须自包含：正文不写 `Parent:` / `Related:` / PR 占位符，直接写清它要什么。
- 关闭前 `Done when` 的 checkbox 全勾。

### PR

- 标题纯英文（conventional commit 风格）；正文小节标题英文、内容中文。
- 正文按仓库模板（`.github/PULL_REQUEST_TEMPLATE.md`）写：背景 / 改了什么 / 为什么 /
  实现步骤 / 交付记录 / 怎么验证 / 检查清单。
- 关联 issue 用 `Fixes #<n>` 收尾行；draft 阶段用 `Related #<n>`，合并授权前改 `Fixes`。
- 开启或更新 PR 后看 CI 结果到底（`gh pr checks`），红了就修，不等用户来问。
- 被 gate 拦下就修代码，**不改规则**。规则确有缺陷 → 开 issue 交维护者裁决。

### 收尾

- 收尾时清掉：已合并分支、临时 worktree、临时进程、跑完的 dev server。
- 资源及时释放；只保留维护者需要的进程（如用户要看的 web 前端）。
