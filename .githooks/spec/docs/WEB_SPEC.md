# WEB-SPEC · web/UI 组件规范检查文档（canon-owned，随 spec 部署到成员仓）

> 正本：`canon/specs/docs/WEB_SPEC.md`（部署到各仓 `.githooks/spec/docs/WEB_SPEC.md`）。
> 作用：把两份源文档的**全部规范条款**映射到 gate 检查项。被拦/被 WARN 时先读这里，
> 再读 `spec_explain(rule_id)` 看具体检测方式。

## 1. 源文档（规范出处）

| 文档 | 内容 | 各仓位置 |
|---|---|---|
| `ui-component-principles.md` | 组件创建基准：归属三问、弹层分工矩阵、API 五条决议、视觉 token 决议、a11y 决议、gate 式验收 | 各仓 `todo/`（ui-kit 为正本） |
| `web-code-review-guide.md` | 可读性向审查：Dioxus 反模式、结构/状态分层、重构手法、回归防护、Rust 底线 | 各仓 `todo/` |

gate 不重新发明规则——**条款原文在两份文档里**，本文档只做「条款 → 检查项」的映射。

## 2. 检查项映射（全部条款 → 谁负责判）

### 2.1 确定性层（l1，`dioxus_web.py`，零 LLM 成本）

| 条款 | 检查项 | 载体 |
|---|---|---|
| 原则 §4.1 token 唯一来源（硬编码色值违规） | `DIOXUS-RAW-PALETTE` 原始色板类（`bg-zinc-800` / `text-white`…） | `checklist_dioxus_style_scatter`（`--only style`） |
| 同上（#hex 硬编码） | `DIOXUS-HARDCODED-COLOR` | 同上 |
| 同上（成组样式散落，单原子类不提常量——防过度抽象） | `DIOXUS-INLINE-CLASS`（≥72 字符） | 同上 |
| 指南 §2 RSX 三层内可读（嵌套 ~3 层抽组件） | `RSX-NESTED-ELEMENT`（`--only nesting`，ferrite 口径 limit 2） | `checklist_dioxus_rsx_nesting` |
| 指南 §1/§3 单 rsx 纪律 + 文案常量化（H1.3/H3 口径） | `WEB-SINGLE-RSX` / `WEB-I18N-CONSTANT`（`--only spec`） | `checklist_web_spec_deterministic` |
| 指南 层级（components/ 反向依赖 views/） | `WEB-LAYERING-DEPENDENCY`（`--only layering`） | `checklist_web_layering`（项目侧） |

### 2.2 语义层（l2，`web_spec.json` via `jev_rule.py`，11 问句）

| 条款 | 问句 id | 类型 |
|---|---|---|
| 指南 §1 Dioxus 反模式（effect 自循环/忙轮询/effect 干派生/非稳 key/滞后闭包） | `effect_antipattern` | noul |
| 原则 §2.1/§2.4 弹层矩阵（单字段 Popover / 危险 Dialog / 全量 Sheet / 禁右键 / 禁 checkbox 批量） | `overlay_matrix` | noul |
| 原则 §3 API（受控优先 / variant 非 render_x / on_* 禁 handle_* / 四态建模） | `api_contract` | noul |
| 原则 §3 props ≤7（超了拆子组件/参数对象） | `props_bloat` | noul |
| 原则 §1 归属（共享组件禁业务 import） | `ownership_imports` | noul |
| 指南 §2 状态归属三级（组件本地→页级→全局，跨页才上提） | `state_ownership` | noul |
| 指南 §2 rsx 可读（嵌套/分支链/命名动宾） | `rsx_readability` | noul |
| 指南 §3 重构触发（同块两处/30 行子树/长布尔/可测逻辑下沉） | `refactor_trigger` | noul |
| 原则 §5 a11y（testid/role/aria/图标按钮命名/hover 必配 click） | `a11y_gap` | noul |
| 主问题分类 | `web_spec_kind` | choice |
| 严重度（0-3） | `web_spec_severity` | score |

**分工纪律**：能数得清的（色板类/嵌套层数/rsx 计数/中文）归 l1，**不进语义层重复判**；
语义层问句只写 grep 判不了的。色值迁移（把存量 `border-zinc-800` 改 `border-border`）
不在 l1 指挥范围内（H1 零漂移口径）——l1 只管「不许新增」。

## 3. 运行方式与处置

```text
canon check dioxus_style_scatter          # l1 确定性，秒级
canon check web_spec --sla l2             # 语义层（需 jev key；无 key 降级 candidates-raw WARN）
```

- 两个规则都挂 **pre-push / merge**，不挂 pre-commit（热路径零成本）。
- `fail_severity: WARN` + `sla: l2` = **非阻断处置型**：存量债大（ferrite 实测 RAW-PALETTE
  1015 条），先 `--scope changed` 管增量，存量走 merge 全仓审计 + 书面驳回
  （写进 PR 的 Delivery record）。
- 无 `TYPESAFE_API_KEY` 时语义层候选原样降 WARN（召回不丢），绝不静默清零——
  看到 `tier: candidates-raw` 的 finding = 没判，按 intent 条文人工核一遍。

## 4. 三仓适配说明

| 仓 | web 代码 | 特殊点 |
|---|---|---|
| ferrite | `crates/web/**` + `apps/*-web/**` | 已有仓内 `custom/web_refactor.json`（ferrite 页面 crate 专用 13 问句）；本 spec 是跨仓通用层，两者并存不冲突（custom 受 canon-sync 保护） |
| omenic | `crates/web-ui/**` | 层级约束实测：views/ 内放组件是刻意设计，`layering` 只判反向 import |
| ui-kit | `src/**`（kit 本体） | 共享组件 crate：`ownership_imports` 问句主要服务它（kit 禁业务 import） |

## 5. 验收口径

- 新增/修改 web 代码：`canon check --sla l2` 无新增 FAIL/WARN（或已书面驳回）。
- 被拦时：`spec_explain(web_spec)` / `spec_explain(dioxus_style_scatter)` 读检测细节，
  按 §2 映射回源文档条款改，**不要让检查闭嘴**（改 yaml/降严重度 = 改约束，走 issue）。
