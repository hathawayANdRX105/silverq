# ferrite gate 检查规则总览

规则只在 `.githooks/spec/checklist_*.yaml` 里，本文件是对照清单。**新增或修改规则后必须更新本文件。**

手动跑：`gate check [names...] --sla {l1|l2|l3} [--json]`。不带名字则列出当前 SLA 层级下的全部检查项。

## SLA 分层

- **l1 结构层**：确定性检查（grep / clippy / cargo-machete），零或低 token。FAIL 是硬门槛。
- **l2 语义层**：轻量语义（重复代码、跨 crate 影响面），秒级。FAIL 是硬门槛。
- **l3 LLM 层**：按需调用，输出 `score` 与 `confidence` 供开发 agent 自行判断，**不阻断合并**。

`gate check` 默认只跑 l1。重规则（clippy / dep_hygiene / duplication / crg_impact）设 `hooks: [merge]`，不拖慢日常提交。

## 规则清单

| 名字 | SLA | 触发 | 严重度 | 检测内容 |
|---|---|---|---|---|
| `structure_check` | l1 | pre-commit, pre-push, merge | FAIL | crate 分层与数据边界（面板禁直接 `use mock::`，禁旧嵌套路径） |
| `shared_components_check` | l1 | pre-commit, pre-push, merge | FAIL | ≥2 个 page 共用的组件必须放共享 crate，page 内禁 `src/ui.rs` |
| `no_nested_types` | l1 | pre-commit, pre-push, merge | FAIL | 禁止在 `fn` 体内定义 `struct` / `enum` |
| `no_nested_worktree` | l1 | pre-commit, pre-push, merge | FAIL | 禁止 `.wt/` 下嵌套 worktree（历史事故：13 层嵌套 + 321G 产物） |
| `rust_no_process_cmd` | l1 | pre-commit, pre-push, merge | FAIL | HTTP 调用走 reqwest，不要 subprocess 拉 curl / wget |
| `rust_tests_in_tests_dir` | l1 | pre-commit, pre-push, merge | FAIL | 测试放同层 `tests/`，禁止在 `src/` 留 `#[cfg(test)]` |
| `copy_constants_check` | l1 | pre-commit, pre-push, merge | WARN | 文案常量：同一中文字面量复用 2+ 次要抽 const |
| `tests_check` | l1 | pre-commit, pre-push, merge | WARN | 测试代码划分与命名 |
| `rust_no_dead_code_allow` | l1 | pre-commit, pre-push, merge | WARN | 合并前清理 `#[allow(dead_code)]`（同行带 `//` 理由则放行） |
| `rust_no_empty_module` | l1 | pre-commit, pre-push, merge | WARN | 微型空文件（≤2 行且无实现），考虑合并到上层 mod |
| `rust_no_cfg_test_in_tests_dir` | l1 | pre-commit, pre-push, merge | WARN | `tests/` 目录里不需要 `#[cfg(test)]` |
| `rust_test_no_assert` | l1 | pre-commit, pre-push, merge | WARN | 测试函数必须含断言 |
| `rust_todo_needs_issue` | l1 | pre-commit, pre-push, merge | WARN | TODO / FIXME 必须挂 issue 号（`// TODO(#123):` 或 `todo!("TODO(#123): ...")`） |
| `hardcoded_secret` | l1 | pre-commit, pre-push, merge | WARN | 硬编码密钥 / 密码 / Token（PCRE，5 语言） |
| `stale_api` | l1 | pre-commit, pre-push, merge | WARN | 废弃 Rust API（`uninitialized` / `try!` / `ONCE_INIT`） |
| `slop_comment` | l1 | pre-commit, pre-push, merge | WARN | AI 风格注释（`Step 1:` / `This function` / `该函数`…） |
| `clippy` | l1 | merge | FAIL / WARN | rustc 编译错误与 `unused_*` / `dead_code` 判 FAIL；`collapsible_if` 等风格判 WARN |
| `dep_hygiene` | l1 | merge | WARN | `cargo-machete` 未使用依赖 |
| `duplication` | l2 | merge | WARN | 跨文件 4+ 连续行重复块 |
| `crg_impact` | l2 | merge | WARN | diff 跨 3+ crate 改动，提示耦合 |
| `pr_labels` | l1 | merge | FAIL | PR 至少挂 1 个 type label（bug/feature/chore/refactor/tests/documentation/epic）；标题命中域关键词但缺域 label 时给出建议（gh api 取数，取数失败输出"跳过、请人工核对"） |
| `pr_crg_review` | l1 | merge | FAIL | PR 讨论区需留有 CRG（code-review-graph）审查结论；若记录提到过问题/风险，需附修复/回应记录（Fix/采纳/驳回 + commit 或验证结论）。只统计 PR 创建后的评论（gh api 取数，取数失败输出"跳过、请人工核对"） |
| `doc_sync` | l1 | pre-commit/push/merge | FAIL | 有 Cargo.toml 的功能 crate 必须配 README.md；域目录（crates/<domain>/）必须有 README；根 README.md 必须存在 |
| `code_doc` | l1 | pre-commit/push/merge | WARN | 公共 API 缺 `///` rust doc、模块头缺 `//!`（只查本次 PR diff 触碰的 .rs，不追责存量） |

## 路径无关性（重要）

harness 一律扫仓库根加 `--exclude-dir`，**不假设 crate 嵌套深度**。

ferrite 是两层布局（`crates/<domain>/<crate>/src`），而规则原先写死一层的 `crates/*/src/`，导致 45 个 crate 里只有 1 个被扫到——其余 44 个的代码从未被检查，gate 却报 `ALL PASS`。静默失效比直接报错更危险，所以新增规则不得再写死目录层级。

## 明确不做的（不写规范，不检查）

- class：不抽文件、不抽 const，直接写 rsx（改动频繁，就近维护）
- i18n：单语言阶段不上 fluent / rust-i18n
- rsx 语法：编译器通过即可
- constants crate：不建独立 crate，文案按共享范围就近 const

## 外部工具依赖

- `code-review-graph`（CRG）：结构分析与变更影响检测
- `ocr`（OpenCodeReview CLI）：LLM 代码审查，按模块分批跑
- `cargo-machete`：未使用依赖检测
- 缺失的工具按 yaml 的 `optional` 处理（默认 WARN 跳过）
