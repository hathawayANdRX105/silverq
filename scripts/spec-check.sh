#!/usr/bin/env bash
# scripts/spec-check.sh — 规范门禁的结构规则入口（rust-specs）。
#
# 用法（日常入口是 justfile 的 `just spec-check`，本脚本是它的实现）：
#   scripts/spec-check.sh                       # deterministic 结构规则（changed 口径）
#   scripts/spec-check.sh --scope all           # 全仓结构规则
#   scripts/spec-check.sh --class advisory      # advisory 层（重复文件 + git 信号）
#   scripts/spec-check.sh --class semantic      # 语义层（需 jev 端点）
#
# 其余参数透传给底层命令。`cargo xtask` 在 .cargo/config.toml 里定义，指向
# rust-specs 的 checks crate（structure 子命令）。
#
# 退出码: 底层命令的退出码
#   0 = 无 finding
#   1 = 有 FAIL finding
#   2 = 环境不可用（缺 rust-specs / 缺基线 / 工具链缺失）
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel 2>/dev/null)
cd "$ROOT" || exit 1

CLASS="deterministic"
SCOPE="changed"
BASE="origin/master"
ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
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
    *)
      ARGS+=("$1")
      shift
      ;;
  esac
done

# 结构规则（+ 语义层）：rust-specs checks。
# --scope changed 只查本次改动（相对 merge-base）；--all 查全仓。
exec cargo xtask structure \
  --class "$CLASS" \
  --scope "$SCOPE" \
  --base "$BASE" \
  --root "$ROOT" \
  "${ARGS[@]}"
