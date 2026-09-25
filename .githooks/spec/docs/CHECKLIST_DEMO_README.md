# Checklist 检查清单 — 用户使用指南

`.githooks/spec/checklist_*.yaml` 是项目级 LLM 检查清单的入口。每份 yaml = 一条检查，gate 会按字典序全部跑（可改名加前缀控顺序：`00_`、`10_`、`99_`）。

## 三步上手

### 1. 复制 demo

```bash
cp .githooks/spec/checklist_no_debug_log_demo.yaml \
   .githooks/spec/checklist_my_rule.yaml
```

### 2. 改 harness 段

最常见形式：调 LLM CLI（`claude` / `codex` / `ollama run` 都行）。

```yaml
harness:
  command: "sh"
  args:
    - "-c"
    - "cat | claude --model haiku --print --output-format json '<你的 prompt 模板>'"
optional: true           # harness 不在 → WARN 跳过
timeout: 30              # 秒

`prompt` 里**必须**告诉模型：
- 看的是 git diff（`mode: diff`）还是单个文件（`mode: file`）
- 命中什么要报、什么不算
- 输出**只**允许是 JSON 数组，每条元素字段：`id` / `severity` / `path` / `line` / `message`
- 没有命中输出 `[]`

### 3. 选 mode

| mode | 输入 | 适合 |
|---|---|---|
| `diff` | git diff 全文（stdin） | 风格、新增代码扫描、debug 残留 |
| `file` | 每个变更文件单独 stdin | 架构放置、巨型文件、模块分层 |

`diff` 快 + 省 token；`file` 重 + 准。`pre-commit` 用 `diff`；`pre-push` / `merge` 可以混。

### 4. 选触发钩子

```yaml
hooks: [pre-commit, pre-push, merge]
```

- pre-commit 慢检查 → 用户体验差；建议只放 diff 检查
- pre-push 折中
- merge 手动触发，最重检查放这

## 输出协议

harness stdout **必须**是 JSON 数组（即使一条）：

```json
[
  {"id": "DBG-01", "severity": "FAIL", "path": "src/foo.rs", "line": 42, "message": "println! left"}
]
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `id` | ✓ | 建议 `<代号>-<seq>`；gate 会拼成 `<yaml文件名>.<id>` |
| `severity` | ✓ | `FAIL` / `WARN` / `INFO`；与 yaml 的 `fail_severity` **就高取大** |
| `path` | ✗ | 相对仓库根 |
| `line` | ✗ | 行号（diff 模式下从 diff 头部提取） |
| `message` | ✓ | 原因（人类可读） |

## 调试（不调真 LLM）

用 mock harness 验证 gate 管道：

```bash
# 设预制 findings：
export CHECKLIST_DEMO_FINDINGS='[{"id":"X-01","severity":"WARN","line":1,"message":"mock finding"}]'
# 跑 gate：
gate pre-commit
# 或直接试 mock：
./.githooks/spec/CHECKLIST_DEMO_MOCK.sh
```

把 `checklist_*.yaml` 的 `harness.command` 临时改成 `sh ./.githooks/spec/CHECKLIST_DEMO_MOCK.sh` 即可端到端验证，不花 LLM token。

## 在新项目启用

```bash
# 1. 复制 omenic 的 .githooks 整目录
cp -r ~/projects/omenic/.githooks ~/projects/<your-project>/

# 2. 装 gate 二进制 + 初始化
cd ~/projects/<your-project>
gate init
# 3. 调整 checklist_*.yaml（删 demo，改 prompt 适配你的项目）
# 4. 验证
gate pre-commit   # 当前 staged
gate pre-push     # 当前 push
gate merge OWNER/REPO 123   # 手动合并前
```

## 详细规范

见 [CHECKLIST_SPEC.md](./CHECKLIST_SPEC.md)。
