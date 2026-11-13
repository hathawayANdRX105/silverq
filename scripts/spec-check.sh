#!/usr/bin/env bash
# scripts/spec-check.sh — 规范门禁统一入口（rust-specs）。
#
# 用法:
#   scripts/spec-check.sh                       # deterministic 结构规则（changed 口径）
#   scripts/spec-check.sh --scope all           # 全仓结构规则
#   scripts/spec-check.sh --class advisory      # advisory 层（重复文件 + git 信号）
#   scripts/spec-check.sh dylint                # dylint 语法 lint（通用规则，changed 口径）
#   scripts/spec-check.sh dylint --all          # 全仓 dylint
#
# 其余参数透传给底层命令。`cargo xtask` 在 .cargo/config.toml 里定义，指向
# rust-specs 的 checks crate（structure 子命令）；dylint 走 scripts/lint-dylint.sh。
#
# 退出码: 底层命令的退出码
#   0 = 无 finding
#   1 = 有 FAIL finding
#   2 = 环境不可用（缺 rust-specs / 缺基线 / 工具链缺失）
set -euo pipefail

MODE="structure"
CLASS="deterministic"
SCOPE="changed"
BASE="origin/master"
ROOT=""
ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    dylint)
      MODE="dylint"
      shift
      ;;
    --class)
      CLASS="$2"
      shift 2
      ;;
    --scope)
      SCOPE="$2"
      shift 2
      ;;
    --base)
      BASE="$2"
      shift 2
      ;;
    --root)
      ROOT="$2"
      shift 2
      ;;
    --all)
      SCOPE="all"
      shift
      ;;
    *)
      ARGS+=("$1")
      shift
      ;;
  esac
done

if [[ -z "$ROOT" ]]; then
  ROOT="$(git rev-parse --show-toplevel)"
fi
cd "$ROOT"

if [[ "$MODE" == "dylint" ]]; then
  exec bash scripts/lint-dylint.sh "${ARGS[@]}"
else
  # 结构规则（+ 语义层）：rust-specs checks。
  # --scope changed 只查本次改动（相对 merge-base）；--all 查全仓。
  exec cargo xtask structure \
    --class "$CLASS" \
    --scope "$SCOPE" \
    --base "$BASE" \
    --root "$ROOT" \
    "${ARGS[@]}"
fi
