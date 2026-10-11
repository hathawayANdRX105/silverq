# `.githooks/spec/` 目录布局（正本）

规则文件住错目录会让它**静默失效**，所以布局是硬约定，不是审美问题。

## 布局

```
.githooks/
├── canon                 # canon 二进制（canon-sync 分发）
└── spec/
    ├── dispatch.yaml         # hook → topic 路由表。engine 从根读，必须留根
    ├── severity_overrides.yaml # 规则注册表 + 覆盖。同上，必须留根
    ├── quality/              # checklist_*.yaml —— 通用确定性检查规则
    ├── dioxus/               # checklist_dioxus_*.yaml + web_spec.json —— dioxus 家族
    │                         # （canon-owned，只发给有 dioxus 证据的仓，见下）
    ├── code/                 # code_<lang>.yaml —— 语言级 lint 规则（topic: code）
    ├── cleanup/              # cleanup_*.yaml —— 分支/测试/文档清理（topic: cleanup）
    ├── github/               # github_*.yaml —— issue/PR/review 策略（topic: github/<名>）
    ├── workspace/            # workspace_*.yaml —— 工作区卫生（topic: workspace）
    ├── harness/              # harness 脚本、jev/semantic/ccn 载荷
    ├── docs/                 # SPEC_OVERVIEW / CHECKLIST_SPEC / WEB_SPEC 等人查文档
    └── custom/               # 项目专有：canon-sync push 永不触碰
```

## 为什么必须住子目录

1. **路由**：`catalog::topic_of()` 要求相对路径里有 `/` 才能算出 topic。根层文件
   拿不到 topic → 没有 `hooks:` 的规则**永远不执行**（`github_*` / `workspace_*` /
   `code_*` / `cleanup_*` 的根层副本就是死的）。
2. **目录即路由**：`catalog::topic_of()` 里 `checklist_*.yaml` 一律归 `checklist`
   这个跨目录 topic（`crates/gate/src/catalog.rs`），所以 checklist 住在哪个子目录都会被
   `canon check` 与 hooks 收上来；根层文件因为算不出 topic → 对目录与 hooks 都不可见。
   `custom/` 不参与加载（项目自留地，放样张与私有配置不会被跑）。
3. **重复层**：`engine::find_specs()` 递归收集 `checklist_*.yaml` 且**不去重**，同一
   规则在两个子目录各一份 = 跑两遍、各用各的配置。搬家必须同时清旧副本（canon-sync 的
   `MOVED_RULES` 负责这件事）。

## `canon-sync push` 对根层文件做什么

| 根层文件 | 处置 |
|---|---|
| 名字命中上表前缀，子目录无同名 | **移动**进子目录（规则从死变活） |
| 子目录有同名且内容一致 | **删根层副本**（去重复层） |
| 子目录有同名但内容不一致 | **留根层**，输出 `CONFLICT`，等人工合并 |
| `RETIRED_RULES` 里的名字 | **整份删除**（递归，`custom/` 除外） |
| `dispatch.yaml` / `severity_overrides.yaml` | 留根（engine 从根读） |
| 认不出的名字（项目自有配置、`llm-checklist-harness.sh`） | 不动 |

`llm-checklist-harness.sh` 留在根层是有意的：多个 `checklist_*.yaml` 用绝对路径
`$REPO/.githooks/spec/llm-checklist-harness.sh` 引用它，搬家会打断引用。

## `canon-sync push` 分发哪些目录

| 机制 | 位置（`scripts/canon-sync`） | 行为 |
|---|---|---|
| canon-owned | `OWNED_DIRS` / `OWNED_FILES` | 整目录/单文件分发；成员仓自有的规则文件不在名单里 = PROJECT-ONLY，push 不碰 |
| 适用面门 | `GATED_DIRS` | 目录级判据：`spec/dioxus` 要求目标仓有 dioxus 依赖或 `rsx!` 宏（`has_evidence()`，与 harness 的 `*-NO-CORPUS` 自查同口径）。不满足就**不分发**，别让别仓白背永不命中的进程 |
| 搬家清场 | `MOVED_RULES` | 规则换了子目录后，push 删掉成员仓旧位置的副本；内容与正本不一致 → 不删，报 `CONFLICT` 让人裁决；正本还没落地 → 不动唯一副本 |
| 退役 | `RETIRED_RULES` | canon 已删的规则，递归从成员仓清掉 |
| 项目自留地 | `PROTECTED = ["spec/custom"]` | 豁免名单（`css_token_allowlist.txt`、`css_guard_skip.txt`）等住这里，push 永不覆盖 |

所以 canon 侧目录改名 = 一次分发事件：`specs/<新目录>/` 落正本 → `.githooks/spec/<新目录>/`
同步镜像（那是 push 的实际源）→ `OWNED_DIRS`/`MOVED_RULES` 各加一行 → 对每个成员仓跑
`canon-sync status` 看 DUPLICATE-LAYER / MOVED-STALE 清零。

## 迁移时的行为变化

把根层规则移进子目录会**激活**之前死掉的规则（它们开始按 dispatch 路由执行）。
所以搬迁后必须跑一次 `canon check` / `canon pre-commit`，把新冒出来的 finding
逐条处置，不能当成回归直接回滚布局。
