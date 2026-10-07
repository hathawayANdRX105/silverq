# `.githooks/spec/` 目录布局（正本）

规则文件住错目录会让它**静默失效**，所以布局是硬约定，不是审美问题。

## 布局

```
.githooks/
├── canon                 # canon 二进制（canon-sync 分发）
└── spec/
    ├── dispatch.yaml         # hook → topic 路由表。engine 从根读，必须留根
    ├── severity_overrides.yaml # 规则注册表 + 覆盖。同上，必须留根
    ├── quality/              # checklist_*.yaml —— 所有确定性检查规则
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
2. **目录即 topic**：`catalog::load()` 只扫 `quality|code|cleanup|workspace|github`
   五个子目录，根层文件对 `canon check` 与 MCP spec 目录**不可见**。
3. **重复层**：`engine::find_specs()` 递归收集 `checklist_*.yaml` 且**不去重**，
   根层与 `quality/` 各一份 = 同一规则跑两次，一次用旧配置。

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

## 迁移时的行为变化

把根层规则移进子目录会**激活**之前死掉的规则（它们开始按 dispatch 路由执行）。
所以搬迁后必须跑一次 `canon check` / `canon pre-commit`，把新冒出来的 finding
逐条处置，不能当成回归直接回滚布局。
