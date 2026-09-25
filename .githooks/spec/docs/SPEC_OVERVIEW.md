# .githooks 规范总览

> 人手查规则/阈值/怎么加规则的总入口见 `.githooks/GATE_HANDBOOK.md`。

```
.githooks/
├── hooks/                     # git hooks 入口（core.hooksPath = .githooks/hooks）
│   ├── pre-commit            # bash 包装 → exec gate pre-commit（CM-01/02/03 + workspace + code + checklist）
│   ├── pre-push              # bash 包装 → exec gate pre-push（workspace + code + checklist）
│   └── merge                 # bash 包装 → exec gate merge（PR + reviews + cleanup + CRG + ocr + checklist）
├── spec/                      # 规则配置（改规则只改这里，不改脚本）
│   ├── SPEC_OVERVIEW.md      # 本文件（规范总览）
│   ├── dispatch.yaml         # 钩子→主题映射（哪个钩子跑哪些检查）
│   ├── github_issues.yaml    # Issue 规则（IS-* 检查项）
│   ├── github_pull_requests.yaml  # PR 规则（PR-* 检查项）
│   ├── github_reviews.yaml   # Review 评论格式（RV-* 检查项）
│   ├── code_{rust,go,javascript,typescript,python,bash}.yaml  # 代码 lint 参数
│   ├── checklist_*.yaml      # 项目级 LLM 检查清单（CK-*；harness = 任意可执行文件；详见 CHECKLIST_SPEC.md）
│   ├── workspace_{tree_hygiene,file_placement}.yaml           # Rust workspace 检查参数
│   ├── cleanup_branch_cleanup.yaml                            # CL-01 分支清理参数
│   ├── cleanup_tests_{rust,go,javascript,bash}.yaml          # CL-02 测试代码检查参数
│   ├── cleanup_docs_hygiene.yaml                             # CL-03 文档卫生检查参数
│   ├── CHECKLIST_SPEC.md      # Checklist 详细规范（yaml schema + harness 协议）
│   └── CHECKLIST_DEMO_README.md # Checklist 用户使用指南
├── GITHUB_ISSUE_PR.md         # Issue/PR 创建指南（含关联机制）
├── PR_DEV_WORKFLOW.md         # PR 开发工作流指南（含 CRG + ocr 审查流程）
└── WORKFLOW.md                # 工作隔离规范（.wt/ worktree 分支目录）

```

### 外部工具依赖

- `code-review-graph`（CRG）：结构分析/变更影响检测（`detect-changes --brief --base main`）
- `ocr`（OpenCodeReview CLI）：AI 代码审查（`review --format json --audience agent`）
- `gh`（GitHub CLI）：所有 GitHub API 操作入口
- 任意 LLM CLI（`claude` / `codex` / `ollama` 等，被 checklist harness 调用；非必须，缺失按 `optional` 处理）

## 本文档用途

本文档是 gate 全部检查规则的**唯一总览**，供人/agent 对照审查：

- 规则**只**在 `.githooks/spec/*.yaml`（参数）和 Rust 校验器（逻辑）两处，改规则只改 spec
- **新增/修改规则后必须更新本文档**
- 规则编号采用**主题前缀 + 连续编号**（IS/PR/RV/GT/WS/CD/CL/CM/CK）
- commit 标题规则 CM-01/02/03 实现在 `gate pre-commit`，语义与 PR-01/PR-02 对齐

## 主题一：GitHub 规则（IS/PR/RV）

参数在 `spec/github_*.yaml`，逻辑在 gate 二进制。

### Issue 规则（IS-01 ~ IS-16）

- IS-01 必填段完整性（Goal/Background/Done when/Suspected areas/Out of scope/How to observe success）— FAIL
- IS-02 Suspected areas 非空 — WARN
- IS-03 body 聚焦（多 H1 提示）— WARN
- IS-04 Done when 必须 checkbox、禁 table — FAIL
- IS-05 标题中文 — FAIL
- IS-06 heading 英文 — FAIL
- IS-07 正文中文 — FAIL
- IS-08 反引号中的仓库路径存在性 — WARN
- IS-09 sub 禁 cross-reference（Depends on/Blocks/Related #/Parent PR）— FAIL
- IS-10 sub 禁 PR 占位符 — FAIL
- IS-11 parent 禁 Done when — FAIL
- IS-14 type label + keyword label 建议 — WARN
- IS-15 关闭时 Done when 全勾（关闭前检查）— FAIL
- IS-16 标题全角括号、禁词、乱码 — FAIL

### PR 规则（PR-01 ~ PR-12）

- PR-01 标题英文（禁 CJK）— FAIL
- PR-02 Conventional Commit 格式 — WARN
- PR-03 必填 body 段完整性 — FAIL
- PR-04 heading 英文、What 段中文 — FAIL/WARN
- PR-05 一个 PR 一个主 issue（Fixes 数量）— WARN
- PR-06 label 存在性 + type label — FAIL
- PR-07 Construction plan/Checklist 至少 2 个 checkbox — FAIL
- PR-08 分支前缀合法 — FAIL
- PR-10 关联机制：Part of/Related 纯文本不产生关联（INFO）；Fixes #N 是 parent issue（epic）时提示用 sub-issue 层级链（WARN）— INFO/WARN
- PR-11 合并前 PR 内 checkbox 全勾（merge 时检查）— FAIL
- PR-12 合并留言理由（merge 时必须 --body）— FAIL

### Review 规则（RV-01 ~ RV-06）

- RV-01 禁 checkbox — FAIL
- RV-02 reply 用词合法（Fix/Block/Resolve/Note/Withdraw/Supersede）— WARN
- RV-03 reply 详细程度 — WARN
- RV-04 CRG/Inline Review 前缀格式 — FAIL
- RV-05 CRG Review 存在 — FAIL
- RV-06 inline findings 有回复 — WARN

## 主题二：拦截门（GT-01 ~ GT-07）

部署为 `~/.local/bin/gh`（argv[0]==gh 时拦截）。

- GT-01 issue create 前校验（调 IS-*）；支持 `--disable-check` 逃生门；sub mode 被拒且看起来是 epic 时提示加 `--label epic` — FAIL 拒 — 触发：gh issue create
- GT-02 pr create 前校验（调 PR-*）— FAIL 拒 — 触发：gh pr create
- GT-03 sub 自动挂载 parent（addSubIssue POST + verify_mount 重试）— 挂载失败 WARN（issue 已创建不回滚，rc=2）— 触发：gh issue create
- GT-04 issue close 前只查 Done when 段 checkbox 全勾 + 必须 --comment 理由（Implementation Order 进度格不拦）— FAIL 拒（开关 `close_done_when_gate`/`close_requires_comment`，严重度可经 severity_overrides 降级）— 触发：gh issue close
- GT-04b issue close 前 PR 关联检查：epic 豁免（完成信号是 GT-06 sub 全关）；非 epic 无关联仅提示不阻塞 — WARN — 触发：gh issue close
- GT-05 pr merge 前 checkbox 全勾 + 关联 Fixes issue Done when 全勾（epic 目标豁免，由 GT-06 保障）+ --body 理由 + squash 标题 CM-01/CM-02 — FAIL 拒（开关 `merge_checkbox_gate`/`merge_fixes_gate`/`merge_requires_body`/`merge_title_gate`）— 触发：gh pr merge
- GT-06 epic close/merge 前所有 sub-issues 已关闭（开关 `epic_sub_issue_gate`；sub 查询失败仍 fail-closed 硬拒，不可配）— FAIL 拒 — 触发：gh issue close / gh pr merge
- GT-07 merge 后自动在 PR 留言 + 删除本地 head 分支（安全模式）— 行为（无拦截）— 触发：gh pr merge
- RV-07 有文件改动的 PR merge 前必须 CRG + ocr 审查 — FAIL 阻塞（`github_reviews.yaml merge_review.required: false` 关；`ocr_timeout_secs` 调超时）— 触发：gate merge

参数剥离：`gh_args()` 剥 `--parent`/`--repo`/`-R`；`arg_repo()` 提取 `--repo` 值（issue close 从 --repo 或 cwd 取仓库）。

## 主题三：钩子调度（gate pre-commit / pre-push / merge）

- `gate init` 部署：复制二进制到 `~/.local/bin/gate`（+ 同二进制为 `~/.local/bin/gh`）、设置 `core.hooksPath=.githooks/hooks`、写 hook 模板
- pre-commit：CM-01/CM-02/CM-03（commit 标题格式/CJK/与 PR type 一致）+ workspace（WS-*）+ code（CD-*）
- pre-push：workspace + code（cargo 不传 target、ruff 排除 .githooks、file_placement 忽略 .githooks/）
- merge（手动 `gate merge <owner/repo> <pr_number> [--dry-run]`）：PR + reviews + cleanup + RV-07（CRG + ocr）

### Commit 标题规则（CM-01 ~ CM-03）

- CM-01 conventional commit 格式（feat/fix/docs/style/refactor/perf/test/build/ci/chore/revert）— FAIL
- CM-02 禁 CJK（应为英文，同 PR-01）— FAIL
- CM-03 分支有关联 open PR 时 type 与 PR 标题一致（gh api，失败/无 PR 跳过）— FAIL


## 主题四：Code（CD-01 ~ CD-06，Rust code lint dispatcher）

- CD-01 rust：cargo fmt --check（spec/code_rust.yaml）— FAIL（工具缺失 → WARN 跳过）
- CD-02 go：gofmt -l（spec/code_go.yaml）— FAIL（工具缺失 → WARN 跳过）
- CD-03 javascript：eslint（spec/code_javascript.yaml）— FAIL（工具缺失 → WARN 跳过）
- CD-04 typescript：npx tsc --noEmit（spec/code_typescript.yaml）— FAIL（工具缺失 → WARN 跳过）
- CD-05 python：ruff check --no-cache（spec/code_python.yaml）— FAIL（工具缺失 → WARN 跳过）
- CD-06 bash：shellcheck（spec/code_bash.yaml）— FAIL（工具缺失 → WARN 跳过）

多语言 code YAML 由 `tools/code.rs` 的 `LANGUAGES` 触发；工具缺失 → WARN 跳过（优雅降级：仅 rc==127 或完整 "command not found" 短语视为缺失，防 lint 输出干扰）。公共字段：enabled/command/args/fail_severity/paths_include/paths_exclude。

## 主题五：Workspace（WS-01 ~ WS-02，Rust workspace validators）

- WS-01 tree_hygiene：空目录、单文件目录、深度 > max_depth、孤儿目录（spec/workspace_tree_hygiene.yaml）— WARN
- WS-02 file_placement：forbidden_patterns、expected_locations（spec/workspace_file_placement.yaml）— WARN；`os.walk` 目录剪枝（ignore 含 node_modules/.git/ 等子树时跳过遍历，防大仓库卡死）

## 主题六：Cleanup（CL-01 ~ CL-03，Rust cleanup validators）

- CL-01 branch_cleanup：merged/orphan/temp 分支清理，dry-run 默认（gate merge 调用）；配置 `spec/cleanup_branch_cleanup.yaml` — WARN
- CL-02 tests_check：四语言测试命名/断言数/必需 helper（配置 `spec/cleanup_tests_{rust,go,javascript,bash}.yaml`，gate merge 调用）— WARN
- CL-03 docs_hygiene：全角括号/死链/遗留标记（TODO/FIXME/XXX）/空文件/CRLF/尾随空白（配置 `spec/cleanup_docs_hygiene.yaml`，gate merge 调用）— WARN/INFO

## 主题七：本地审查（RV-07，gate review）

`gate review [--post|--post-inline] [--pr N]`：

1. CRG 结构分析：`code-review-graph detect-changes --brief --base main` → 影响文件/风险分
2. ocr AI 审查：`ocr review --format json --audience agent` → findings（path/start_line/severity/category/content）
3. 输出：终端（默认）/ PR conversation（--post）/ Files changed inline（--post-inline）
4. 审查闭环：findings 留言（有行号）→ 修复 → `Agent 🤖 - Fix:` 逐条回复 → RV-06 校验
5. `[ocr]` 前缀的错误/超时字符串不当 findings（ocr_has_findings 排除），空输出视为审查不可信（fail-closed）

## 主题八：Checklist（CK-01，gate checklist，**已实现**）

- `.githooks/spec/checklist_*.yaml`：项目级 LLM 检查清单；glob 自动发现，按字典序跑
- `mode: diff`（默认）传 `git diff <scope>` 给 harness；`mode: file` 每个变更文件单独传全文
- harness = 任意可执行文件，stdout 必须是 finding JSON 数组（与 code/ocr/CRG 同协议）
- 严重度合并：harness 报的与 yaml `fail_severity` **就高取大**（harness FAIL 永远阻断）
- `optional: true`（默认）harness 缺失 → WARN 跳过；`false` → FAIL
- 实现：`crates/spec/src/tools/checklist.rs`（CK-01 dispatcher） + `gate pre-commit/pre-push/merge` 调度
- 详见 [CHECKLIST_SPEC.md](./CHECKLIST_SPEC.md) 与 [CHECKLIST_DEMO_README.md](./CHECKLIST_DEMO_README.md)

- CK-01 yaml 字段：`enabled` / `hooks` / `match.{paths_include,paths_exclude}` / `mode` / `harness.{command,args}` / `timeout` / `optional` / `fail_severity` — FAIL/WARN/INFO
- CK-02 diff 范围:pre-commit=`git diff --cached`,pre-push=`git diff HEAD`,merge=`git diff origin/main...HEAD`(unified=3)
- CK-03 finding 兼容:单 object / 数组 / "text + [...JSON...]" 末尾数组三种都能解析
- CK-04 `mode: grep`:harness 收空 stdin,跑任意静态检查(grep/find/自定义脚本),finding 自身带 path/line. 适合铁律类规则(禁路径模式、必放位置) — 零 LLM token,毫秒级


## 触发式（lazy）规则映射

- gh issue create → IS-01~16、GT-01（含 --disable-check 逃生门）、GT-03
- gh issue close → GT-04、GT-04b、GT-06（epic；--repo/-R 被 gate 剥离并透传）
- gh pr create → PR-01~10、GT-02
- gh pr merge → PR-11、PR-12、GT-05（含 squash 标题 CM-01/CM-02）、GT-06、GT-07、RV-01~06
- gh pr comment → RV-01~06
- git commit → CM-01、CM-02、CM-03、WS-01、WS-02、CD-01~06、checklist（每 yaml 自身 `hooks` 过滤）
- git push → WS-01、WS-02、CD-01~06、checklist（同上）
> 清单更新顺序：按文件名字典序（加 `00_`/`10_` 前缀可强制提前）。

## 主题十：手动运行检查（gate check）

`gate check [names...]` — 按名字或列出所有 checklist，强制忽略 yaml 的 `hooks:` 过滤，用于调试/CI/按需跑：

- `gate check` → 列出当前 SLA 层级（默认 l1）下的检查项
- `gate check clippy` → 只跑 clippy
- `gate check --sla l2` / `l3` → 解锁更高 SLA 层

每次加/删 checklist yaml，清单自动更新；新规则只需 `cp spec/xxx.yaml .githooks/spec/` 即可。

| 名字 | SLA | 触发 | 严重度 | 检测内容 |
|---|---|---|---|---|
| `hardcoded_secret` | l1 | pre-commit, pre-push, merge | WARN | 硬编码密钥/密码/Token（PCRE, 5 语言） |
| `stale_api` | l1 | pre-commit, pre-push, merge | WARN | 废弃 Rust API（uninitialized/try!/ONCE_INIT） |
| `slop_comment` | l1 | pre-commit, pre-push, merge | WARN | AI 风格注释（步骤/叙述/拖延语 for now·临时·凑合/含糊语 hopefully·估计，5 语言） |
| `ccn` | l1 | pre-commit, merge | FAIL | 函数 ccn 天花板(6) + ratchet 记账：新违规/恶化硬拦，存量 ratchet.tsv 容忍且只许降；lizard 缺失静默跳过 |
| `antislop` | l1 | pre-commit, pre-push, merge | WARN（HIGH→FAIL） | AI slop 五类（Placeholder/Deferral/Hedging/Stub/命名），antislop 二进制；缺失静默跳过 |
| `rust_no_process_cmd` | l1 | pre-commit, pre-push, merge | FAIL | HTTP 调用走 reqwest, 不要 subprocess curl/wget |
| `rust_no_dead_code_allow` | l1 | pre-commit, pre-push, merge | WARN | 合并前清理 #[allow(dead_code)] |
| `rust_no_empty_module` | l1 | pre-commit, pre-push, merge | WARN | 微型空文件, 考虑合并到上层 mod |
| `rust_tests_in_tests_dir` | l1 | pre-commit, pre-push, merge | FAIL | 测试必须同层 tests/ 目录 |
| `rust_todo_needs_issue` | l1 | pre-commit, pre-push, merge | WARN | 注释里 TODO/FIXME 必须关联 issue 号（例 `// TODO(#123):`；不扫字符串/测试） |
| `rust_test_no_assert` | l1 | pre-commit, pre-push, merge | WARN | 测试函数必须含 assert |
| `rust_no_cfg_test_in_tests_dir` | l1 | pre-commit, pre-push, merge | WARN | tests/ 目录里不要 #[cfg(test)] |
| `clippy` | l1 | merge | FAIL/WARN | rustc 错误 + unused/dead_code → FAIL；collapsible_if 等风格 → WARN |
| `dep_hygiene` | l1 | merge | WARN | `cargo-machete` 未使用依赖 |
| `duplication` | l2 | merge | WARN | 跨文件 4+ 连续行重复块（sh+awk, 零依赖） |
| `crg_impact` | l2 | merge | WARN | diff 跨 3+ crate 改动 → 警告耦合 |
| `ferrite_oversize` | l3 | merge | INFO | 大文件/大函数参考分（wildtoken `fast-l`；score/confidence，不阻断） |
| `review_chain` | l3 | pre-push, merge | INFO（harness 透传） | 模型审查三档降级：jev（`TYPESAFE_API_KEY`）→ 小模型（`REVIEW_LLM_*`）→ 无（INFO）；per-question 阈值，p≥fail FAIL；`tier`/`confidence` extra |

close 路径另有 `done_when_judge`（`github_issues.yaml`）：GT-04 机械门过后，Done when 每条过同一套三档模型评审（问题集 `harness/jev_questions_done_when.json`，`default_fail: 0.85`），p(未达标)≥0.85 FAIL 硬拦；任何基础设施失败降 `DWJ-SKIPPED` INFO 不阻断。

### SLA 分层

- **l1 结构层**：零 token，毫秒～分钟级（grep / clippy / 静态分析）。FAIL 硬门槛。
- **l2 语义层**：轻量，秒级（影响面 / 重复检测）。FAIL 硬门槛。
- **l3 LLM 层**：按需，秒~分钟级（`review_chain` 三档降级：jev → 小模型 → 无；`ferrite_oversize` wildtoken `fast-l`）。INFO/score/confidence，不阻断；per-question fail 阈值命中时 FAIL。深度审查自行 `ocr review --format json --audience agent`。

`gate check` 默认只跑 l1；`--sla l2` 或 `l3` 解锁更高层。
l3 默认 hooks: [merge]，本地用 `gate check <l3-name> --sla l3` 触发。

## 更新与校验

- 新增/修改规则：只改 `.githooks/spec/*.yaml` 参数 + 相应校验器逻辑，更新本文档
- gate 改动后：`cargo build --release -p gate-bin` → `upx --best --lzma target/release/gate` → `gate init` 重部署 + `install` 复制为 `~/.local/bin/gh`
- 触发式按上表 lazy 执行，不全局扫描
