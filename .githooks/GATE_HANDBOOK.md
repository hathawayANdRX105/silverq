# gate 手册

gate 是仓库自带的质量门禁：读 `.githooks/spec/*.yaml` 规则 → 调外部命令/LLM → 收 finding → 按严重度放行或拦截。
**加规则只改 yaml，不改二进制。** 本文件是人能查的一手总览；每条规则的参数以对应 `.githooks/spec/checklist_*.yaml` 为准。

## 三层 SLA

| 层 | 性质 | 成本 | 是否阻断 |
|---|---|---|---|
| **l1 结构层** | 确定性（grep / clippy / machete / wc） | 毫秒～分钟，零 token | FAIL 硬拦 |
| **l2 语义层** | 轻量语义（重复块 / 跨 crate 影响面） | 秒级 | FAIL 硬拦 |
| **l3 LLM 层** | 按需 LLM，输出 `score`/`confidence` 参考分 | 分钟级 | **不阻断**，开发 agent 自行判阈值 |

`gate check` 默认只跑 l1；`--sla l2` / `--sla l3` 解锁更高层。重规则设 `hooks: [merge]` 不拖日常提交。

## 规则清单（16 条）

严重度列：`FAIL`=硬拦截，`WARN`=提示不拦，`INFO`=仅参考。

| 规则 | SLA | 自动触发 | 严重度 | 查什么 |
|---|---|---|---|---|
| `hardcoded_secret` | l1 | pre-commit/push/merge | WARN | 硬编码密钥/密码/Token（PCRE，5 语言） |
| `stale_api` | l1 | pre-commit/push/merge | WARN | 废弃 Rust API（`uninitialized`/`try!`/`ONCE_INIT`） |
| `slop_comment` | l1 | pre-commit/push/merge | WARN | AI 风格注释（`Step 1:`/`This function`/`该函数`…） |
| `rust_no_process_cmd` | l1 | pre-commit/push/merge | **FAIL** | HTTP 走 reqwest，禁 subprocess 拉 curl/wget |
| `rust_tests_in_tests_dir` | l1 | pre-commit/push/merge | **FAIL** | 测试放同层 `tests/`，禁在 `src/` 留 `#[test]` |
| `rust_no_dead_code_allow` | l1 | pre-commit/push/merge | WARN | 合并前清理 `#[allow(dead_code)]`（同行带 `//` 理由放行） |
| `rust_no_empty_module` | l1 | pre-commit/push/merge | WARN | 微型空文件（≤2 行且无实现） |
| `rust_no_cfg_test_in_tests_dir` | l1 | pre-commit/push/merge | WARN | `tests/` 里不需要 `#[cfg(test)]` |
| `rust_test_no_assert` | l1 | pre-commit/push/merge | WARN | 测试函数必须含断言 |
| `rust_todo_needs_issue` | l1 | pre-commit/push/merge | WARN | TODO/FIXME 必须挂 issue 号（`// TODO(#N)` 或 `todo!("TODO(#N)")`） |
| `dep_hygiene` | l1 | merge | WARN | `cargo-machete` 未使用依赖（工具缺失则 WARN 跳过） |
| `clippy` | l1 | merge | **FAIL/WARN** | rustc 编译错误 + `unused_*`/`dead_code`→FAIL；`collapsible_if` 等风格→WARN |
| `file_size` | l1 | merge | WARN | 单 `.rs` >1500 行 或 >35KB → 提示按职责拆分（存量宽，清账后可升 FAIL） |
| `duplication` | l2 | merge | WARN | 跨文件 4+ 连续行重复块 |
| `crg_impact` | l2 | merge | WARN | diff 跨 3+ crate 改动，提示耦合 |
| `ferrite_oversize` | l3 | merge | INFO | 大文件/大函数参考分（wildtoken `fast-l`，带 `score`/`confidence`，不阻断） |
| `pr_labels` | l1 | merge | **FAIL** | PR 至少挂 1 个 type label（`bug`/`feature`/`chore`/`refactor`/`tests`/`documentation`/`epic`）；标题命中域关键词（proxy/channel/catalog/admin/tavern/gateway）但缺对应域 label 时给出建议。gh api 取数，取数失败输出"跳过、请人工核对"（不静默假绿） |
| `pr_crg_review` | l1 | merge | **FAIL** | PR 讨论区需留有 CRG（code-review-graph）审查结论；若记录提到过问题/风险，需附修复/回应记录（Fix/采纳/驳回 + commit 或验证结论）。只统计 PR 创建后的评论。gh api 取数，取数失败输出"跳过、请人工核对" |
| `doc_sync` | l1 | pre-commit/push/merge | **FAIL** | 有 Cargo.toml 的功能 crate 必须配 README.md；域目录（crates/<domain>/）必须有 README；根 README.md 必须存在 |
| `code_doc` | l1 | pre-commit/push/merge | WARN | 公共 API 缺 `///` rust doc、模块头缺 `//!`（只查本次 PR diff 触碰的 .rs，不追责存量） |

## 怎么跑

**二进制**：仓库内置 `.githooks/gate`（已 upx 压缩，~0.7MB）。hook 先找 PATH 里的 `gate`，再回退到 `.githooks/gate`，新克隆的人 `git config core.hooksPath .githooks/hooks` 即可跑，不必单独安装。

**自动**（已挂在钩子上，本机 `core.hooksPath=.githooks/hooks`）：`git commit` → pre-commit；`git push` → pre-push；`gate merge <repo> <pr>` → merge（含 checklist 全量）。
`RESULT: FAIL` 且存在 FAIL 级 finding → 退出码 1 → 对应 git 操作被拦截。

**手动**（调试 / CI / 按需）：
```text
gate check                       # 列出当前 l1 层全部规则（列表，不执行）
gate check clippy file_size     # 只跑指定规则
gate check --sla l3             # 解锁到 l3（含 LLM 参考层）
gate check --sla l3 --json      # 机器可读，带 score/confidence extra，给开发 agent 消费
```

## 怎么加一条规则

拷一份模板到 `.githooks/spec/checklist_<名字>.yaml`，填参数：

```yaml
enabled: true
hooks: [pre-commit, pre-push, merge]   # 重活写 [merge]
sla: l1                                  # l1 确定性 | l2 语义 | l3 LLM(带分参考)
fail_severity: WARN                      # 兜底严重度；FAIL 才阻断
mode: grep                               # diff | file | grep(静态,自己扫)
match:
  paths_include: ["**/*.rs"]
  paths_exclude: ["target/", ".wt/"]
harness:
  command: "sh"
  args: ["-c", "<扫仓库根 + 输出 finding JSON 数组>"]
optional: true                           # 工具缺失时 WARN 跳过
timeout: 30
```

stdout 必须是 finding JSON 数组：`{"id","severity","path","line","message"}`（L3 可多带 `score`/`confidence`）。

**可移植标准（强制遵守，见 `CHECKLIST_SPEC.md`「mode: grep 规则编写标准」）：**
1. 扫仓库根 `"$ROOT"`，**禁止**写死 `crates/*/src` 布局（换仓库会静默扫 0 文件、假绿）。
2. grep 用 `--exclude-dir=target --exclude-dir=.wt --exclude-dir=.git`（按目录名，worktree 安全）。
3. find 用 `\( -name target -o -name .git -o -name .wt \) -prune -o ...`，**禁止** `-not -path "*/.wt/*"`（全路径 glob 在 `.wt/` worktree 下会把自己全排除）。
4. 跨语言测试文件命名一并 `--exclude`（`*_test.go`/`*.spec.ts` 等，`--exclude-dir=tests` 挡不住同目录测试）。

## 怎么豁免

- 改该规则 yaml 的 `fail_severity`（如把 `slop_comment` 从 WARN 降 INFO）。
- 全仓统一：`.githooks/spec/severity_overrides.yaml` 按 `规则ID` 覆盖严重度。
- 单条放行：`git commit --no-verify`（不推荐，绕过全部钩子）。

## 路线图（已知短板，未启用）

| 项 | 工具 / 做法 | 状态 |
|---|---|---|
| 注释存在性门禁 | `RUSTFLAGS="-W missing_docs"`（public 59 处存量）；`clippy::missing_docs_in_private_items`（更严） | 存量清账前按 crate 灰度启用 |
| 测试强度 | `cargo-mutants` nightly（验证 agent 测试是否真在检验，抓自证测试） | 待接入 |
| 质量曲线 | `gate check --json` 每次 commit 落 jsonl（clippy 数/LOC/CRG risk/findings 分布） | 待接入 |
| 函数复杂度 | `clippy::cognitive-complexity`（本机 rustc 1.98 已有，restriction 组） | 待定阈值 |
| 模块循环依赖 | `cargo-modules dependencies --lib --acyclic`（工具未装） | 待装 |
