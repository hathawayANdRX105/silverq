# Checklist Spec — 项目级 LLM 检查清单

`.githooks/spec/quality/checklist_*.yaml` 走的是和 `code/code_*.yaml` 同构的「读 yaml → 调外部 harness → 收 finding JSON」管道；唯一区别是把 lint 命令换成 agent harness（任意可执行文件）。

## 设计目标

- **零硬编码检查项**：每条检查 = 一份 yaml，gate 不懂检查含义。
- **harness 任意**：claude / 9router / vLLM / 本地 sh 都行，gate 只调 `argv/stdin → stdout`。
- **复用 finding 协议**：跟 `code.rs` / `ocr` / `code-review-graph` 同一种 `Finding { id, severity, path, line, message }` 结构，统一走 gate 的 FAIL/WARN/INFO 出口。
- **可拓展**：加检查 = 加 yaml，gate 不需要重新发版。

## 文件位置与发现

```
.githooks/
├── spec/
│   ├── dispatch.yaml          # hook → topic（已有；加 checklist topic）
│   ├── quality/checklist_*.yaml  # 检查清单（新增；glob 自动发现，递归）
│   └── ...
```

启动时 `crates/spec/src/tools/checklist.rs` glob `.githooks/spec/checklist_*.yaml`，**顺序按文件名字典序**（用户想让谁先跑就改名加前缀，如 `00_`、`10_`）。

## YAML Schema

```yaml
# .githooks/spec/checklist_<name>.yaml
# name 来自文件名（去 .yaml），与 finding.id 前缀一致

enabled: true                  # 默认 true；false → INFO 跳过
hooks: [pre-commit, pre-push, merge]   # 触发钩子；缺省 = 全部

# 触发条件：必须命中至少一个 match 才跑
match:
  paths_include: ["**/*.rs"]   # git diff 中变更文件命中才跑
  paths_exclude: ["target/", ".wt/", "node_modules/"]

# 模式:diff(快,省 token)/ file(深,全文)/ grep(静态检查,零 LLM)
#  - diff: 传 git diff 给 harness
#  - file: 一次调用传全部命中的变更文件, 按 "===== FILE: rel =====" 标记分隔
#  - grep: harness 收空 stdin,自己跑 grep/find/ripgrep,findings 自带 path/line
mode: diff                     # diff | file | grep,默认 diff

# Harness：任意可执行文件（推荐 sh 包装 LLM CLI）
# stdin/argv 输入见「协议」一节，stdout 必须输出 finding JSON 数组
harness:
  command: "sh"
  args: ["-c", "cat | claude --model haiku --print --output-format json '<prompt>'"]
  # 或者直接可执行：
  # command: "ferrite-check-file-placement"
  # args: ["--strict"]

# 超时（秒）；超时视为 WARN 跳过（不让 commit 卡死）
timeout: 120

# gate 找不到 harness 时：true → WARN 跳过；false → FAIL（强制装机）
optional: true                 # 默认 true

# 失败严重度：FAIL 阻断 commit/push/merge；WARN 仅显示
fail_severity: FAIL            # FAIL | WARN | INFO，默认 WARN
```

### 公共字段（与 code_*.yaml 对齐）

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `enabled` | bool | true | 总开关 |
| `hooks` | string[] | all | 触发钩子白名单 |
| `match.paths_include` | string[] | [] | git diff 中变更文件须匹配至少一条；空 = 不过滤 |
| `match.paths_exclude` | string[] | [] | 命中排除列表则跳过 |
| `mode` | string | diff | `diff` \| `file` \| `grep` |
| `harness.command` | string | — | 必须；harness 入口 |
| `harness.args` | string[] | [] |  |
| `timeout` | int | 60 | 秒 |
| `optional` | bool | true | harness 缺失时 WARN 跳过还是 FAIL |
| `fail_severity` | string | WARN | FAIL \| WARN \| INFO |

## 协议（Harness ↔ gate）

### stdin/argv 输入

gate 根据 `mode` 给 harness 三种输入之一：

**`mode: diff`**（默认）
- stdin = `git diff <scope>` 的完整输出（unified diff）
- argv = yaml 里 `harness.args` 原样；gate 不追加任何参数（旧文档曾写 `--checklist/--scope/--path`，实现里没有）
- 适用：行号级别检查、增量评审、风格校验

**`mode: file`**（一次调用，全部变更文件）
- stdin = 所有命中 match 的变更文件按 `\n===== FILE: <rel> =====\n` 拼接后的完整内容（见 engine.rs Mode::File）
- harness 自行解析标记把 finding 归因到文件；stdout 仍是整体一个 finding JSON 数组
- 适用:函数级检查(ccn 天花板/ratchet)、架构放置、模块分层、API 设计一致性(需要全文上下文)

**`mode: grep`**(v2 新增,零 LLM)
- stdin = 空 (harness 自己跑 grep/find,无需 gate 喂数据)
- argv = 同上
- 适用:铁律类规则(禁路径/禁模式/必放位置) — 不需要 LLM 推理,直接 grep 即可
  - harness 内联 sh:`sh -c 'grep -rn "use mock::" crates/page/*/src/ | jq -R ...'`
  - 或调用 ripgrep / 自定义 Python 脚本
- 严重度:与 harness-reported 取 max;无 finding 即 PASS

### stdout 输出（强制）

**JSON 数组**，每个元素是一条 finding：

```json
[
  {
    "id": "FP-01",
    "severity": "FAIL",
    "path": "crates/page/admin/src/network.rs",
    "line": 763,
    "message": "new use_signal 应紧邻已有 use_signal 块（760-762），而非插入到中段"
  }
]
```

字段：
- `id` (string, required)：建议 `<checklist-name>-<seq>`，最终会拼成 `<checklist-name>.FP-01`
- `severity` (string, required)：`FAIL` | `WARN` | `INFO`；与 yaml `fail_severity` 取**就高**（harness 报 FAIL 永远阻断）
- `path` (string, optional)：相对仓库根；缺省 = 空（全局性问题）
- `line` (int, optional)：行号；缺省 = 0
- `message` (string, required)：中文/英文均可

**容错**：
- stdout 不是合法 JSON → 整个 yaml 视为 ERROR + WARN「harness 输出不可信」
- 部分行解析失败 → 该条丢弃，其余保留
- stdout 空数组 `[]` → 全 PASS

### 退出码

| rc | 含义 |
|---|---|
| 0 | 正常（无论有没有 finding） |
| 2 | harness 自身报错（找不到 config、prompt 模板错等）→ WARN「harness failed」 |

## mode: grep 规则编写标准（可移植 + gitignore 感知 + worktree 安全）

`mode: grep` 的 harness 自己扫仓库。**扫描集一律用 `git ls-files`（git 跟踪文件集），禁止文件系统遍历。**
新加规则照抄，不要各写各的。

1. **扫描集 = git 跟踪文件集，不走文件系统。** `grep -r "$ROOT"` / `find "$ROOT"` 会走 filesystem：
   扫进 gitignored 参考项目、未跟踪产物（`todo/`、`node_modules/`、`.venv`）→ 每笔提交假 FAIL。
   统一改成：
   ```sh
   cd "$ROOT"
   matches=$(git ls-files -z -- '*.rs' | grep -zvE '(^|/)tests/' | xargs -0r grep -nEn '<pat>' 2>/dev/null | head -30)
   ```
   **`.gitignore` 就是排除配置**：目录被 gitignore 即自动出扫描集；按规则细化排除在文件列表上
   `grep -zvE '(^|/)xx/'`（或 pathspec `:(exclude)...`）。

2. **不写死目录层级**（`crates/*/src` 等 omenic 专属布局）：`git ls-files` 输出仓库根相对路径，
   硬编码布局在别的仓库静默扫 0 文件、假绿（比报错更危险）。

3. **跨语言测试文件命名一并排除**（`tests/` 目录挡不住同目录 `*_test.go` / `test_*.py` /
   `*.test.ts` / `*.spec.ts`）：`grep -zvE '(^|/)(test_[^/]*\.py|[^/]*\.(test|spec)\.(ts|js)|_test\.(go|py))$'`。

4. **worktree 安全自动获得**：`.wt/<x>` 检出里 `git ls-files` 给的是该 worktree 的跟踪集，
   旧 `-not -path "*/.wt/*"` 全路径 glob 假绿坑作废。

> 判据：拷到 3 种布局验证——(a) `crates/*/src` 单域，(b) `crates/<domain>/<crate>/src` 两域，
> (c) `apps/*` 非 cargo。三种都应扫到真实文件、`.wt/` worktree 不虚报全绿、gitignored 目录不入扫。

## 触发点（hook 调度）

`dispatch.yaml` 加 `checklist` topic：

```yaml
pre-commit:
  - workspace
  - code
  - checklist      # 新增

pre-push:
  - workspace
  - code
  - checklist      # 新增

merge:
  - github/pull_requests
  - github/reviews
  - cleanup
  - checklist      # 新增（手动 gate merge 时全跑）
```

checklist 自身再用 `hooks:` 字段过滤；topic 进来后**每个 yaml 独立判断**是否在本钩子触发。

## Gate 端实现要点

参考 `crates/spec/src/tools/code.rs::run_lang`，新写 `crates/spec/src/tools/checklist.rs`：

```rust
pub fn run_all(scope: HookScope, target_files: &[PathBuf]) -> Vec<Finding> {
    let yamls = glob_spec("checklist_*.yaml");
    yamls.par_iter()  // rayon 并行
        .filter(|y| y.hooks.contains(scope))
        .filter(|y| y.matches(target_files))
        .flat_map(|y| run_one(y, scope))
        .collect()
}
```

### 实现清单（建议 ~120 行）

1. `pub struct ChecklistSpec { name, enabled, hooks, match_, mode, harness, timeout, optional, fail_severity }`，serde_yaml 派生
2. `glob(".githooks/spec/checklist_*.yaml")` 列举
3. `run_one(spec, scope) -> Vec<Finding>`：
   - 取 `git diff --unified=3 <scope>` → bytes
   - `Command::new(spec.harness.command).args(spec.harness.args).stdin(diff).output()`
   - 解析 stdout JSON → finding；拼上 `<spec.name>.<id>` 前缀
4. `hooks` 字段 → `enum HookScope { PreCommit, PrePush, Merge }`，dispatch 传进来
5. `match.paths_include` 复用 `code.rs::matches_include`
6. finding 严重度合并：`max(spec.fail_severity, finding.severity)`
7. harness 缺失（rc=127）→ 走 `optional`：true=WARN「harness not found, skipped」；false=FAIL

### 不需要新做的事

- 不写 LLM 客户端 — harness 外部，gate 是 dumb pipe
- 不写 prompt 模板 — 用户在 sh 命令里写
- 不写 finding schema — 复用 `Finding { id, severity, path, line, message }`

## Demo 文件

### 1. `checklist_file_placement_demo.yaml`

```yaml
# demo: 文件放置（mode: file，全文件上下文）
enabled: true
hooks: [pre-push, merge]
fail_severity: WARN
match:
  paths_include: ["**/*.rs"]
  paths_exclude: ["target/", ".wt/"]
mode: file
harness:
  command: "sh"
  args:
    - "-c"
    - "cat | claude --model haiku --print --output-format json '检查 Rust 文件放置: (1) 超过 1500 行 → WARN oversized; (2) 同一 crate 下两个 rs 都 impl 同一 trait → WARN duplicate. 输出 JSON 数组.' | jq -c '.[]'"
timeout: 60
```

跑起来（mock harness 时）：
```
$ gate pre-push
[checklist.file_placement_demo] file_placement_demo.FP-01 WARN crates/page/admin/src/network.rs 2952 lines exceeds 1500
```

### 2. `checklist_no_debug_log_demo.yaml`

```yaml
# demo: 增量 diff（mode: diff，快）
enabled: true
hooks: [pre-commit, pre-push, merge]
fail_severity: FAIL
match:
  paths_include: ["**/*.rs"]
mode: diff
harness:
  command: "sh"
  args:
    - "-c"
    - "cat | claude --model haiku --print --output-format json '检查 diff 中的 println!/dbg!/eprintln!. 命中输出 {id: DBG-01, severity: FAIL, line: <从 diff 提取>, message: <原因>}. 不要其他输出.'"
timeout: 30
```

### 3. `CHECKLIST_DEMO_README.md`（用户视角操作说明）

写到 `.githooks/spec/CHECKLIST_DEMO_README.md`，告诉用户：
- 怎么写自己的 checklist yaml（抄 demo）
- 怎么在 ferrite 加 `.githooks/`
- harness 用什么 CLI（claude / 9router / vLLM HTTP wrapper）

## 边界 / 限制

- **LLM 调用慢**：pre-commit 默认每次跑全部命中的 checklist；建议用户在 yaml 里把"重"检查放到 `pre-push` 或 `merge`，`pre-commit` 只跑轻量 diff 检查。
- **超时 = 跳过**：默认 60s 可调；不能让 commit 卡死。
- **无 diff 时跳过**：`git diff` 为空 → INFO「no changes, skipped」，不浪费 LLM 调用。
- **harness 凭据**：claude / 9router 的 API key 走用户 shell 环境（`ANTHROPIC_API_KEY` / `9ROUTER_API_KEY`），gate 不存不传。
- **finding 严重度不可降级**：harness 报 FAIL 永远阻断；yaml 只能声明"最差严重度"，不能把 harness 报 FAIL 降成 WARN。

## 迁移路径

1. 加 `crates/spec/src/tools/checklist.rs`（~120 行）
2. `bin/gate/src/main.rs` 在 PreCommit/PrePush/Merge 路径里调 `run_all`
3. `.githooks/spec/dispatch.yaml` 加 `checklist` topic
4. `.githooks/spec/SPEC_OVERVIEW.md` 加「主题九：Checklist（CK-01）」章节
5. demo yaml + mock harness 脚本（不需真调 LLM；echo mock JSON 即可）
6. `ferrite` 加 `.githooks/` + `gate init` → 跑 `gate pre-push` 验证

## 不做的事（YAGNI）

- ❌ LLM 客户端抽象（HTTP / Anthropic / OpenAI 各一份）— 用户用 CLI 包装
- ❌ Prompt 模板引擎（jinja 之类）— 写在 sh 命令里
- ❌ Finding 缓存（同样 diff 不重复调）— LLM 本身便宜，慢不是 cache 能解的
- ❌ 并行多 harness — rayon 已经够，复杂度不在这
- ❌ Web UI 配 checklist — yaml 就是 UI
- ❌ 自动生成 harness — 不同 LLM CLI 协议差太多
