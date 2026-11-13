#!/usr/bin/env bash
# scripts/lint-dylint.sh — rust-specs dylint 语法 lint（通用规则），按 git diff 收窄。
#
# 用法:
#   scripts/lint-dylint.sh                   # changed：基线到 HEAD + 工作区/暂存
#   scripts/lint-dylint.sh --base <ref>      # 指定基线（默认 origin/master）
#   scripts/lint-dylint.sh --all             # 全 workspace（不按 diff 过滤）
#   scripts/lint-dylint.sh --fail-on-findings  # 有 finding 时退出 1
#   scripts/lint-dylint.sh --dry-run         # 只打印将执行的命令
#
# 环境变量:
#   RUST_SPECS_DIR   rust-specs 路径（默认 /home/hathaway/projects/rust-specs）
#   DYLINT_TOOLCHAIN dylint 用的 toolchain（默认 nightly-2026-08-20，须带 rustc-dev）
#
# 退出码:
#   0 = 无 finding（或 --fail-on-findings 未开）
#   1 = --fail-on-findings 且存在 finding
#   2 = 工具链 / rust-specs / 基线缺失
#
# 为什么需要这个脚本（dylint 6.2 的装配顺序问题）:
#   `cargo dylint --path <rust-specs>` 把 lint 编到 **rust-specs** 的
#   target/release/lib<name>.so（plain 名），再把带 toolchain 后缀的
#   lib<name>@<toolchain>.so 放进 **消费仓** 的 target/dylint/libraries/。
#   但随后的 metadata 步骤查的是消费仓里 **plain 名** 的 .so —— 找不到。
#   ferrite 的 target/dylint/libraries 里两个名字都在（plain + @toolchain），
#   所以那边能跑通。脚本把 plain 名补上，dylint 随后的步骤才拿得到 metadata。
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "✗ 不在 git 仓库内" >&2; exit 2; }
cd "$ROOT"

RUST_SPECS_DIR="${RUST_SPECS_DIR:-/home/hathaway/projects/rust-specs}"
DYLINT_TOOLCHAIN="${DYLINT_TOOLCHAIN:-nightly-2026-08-20}"
BASE="origin/master"
ALL=0
FAIL_ON_FINDINGS=0
DRY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)
      BASE="$2"
      shift 2
      ;;
    --all)
      ALL=1
      shift
      ;;
    --fail-on-findings)
      FAIL_ON_FINDINGS=1
      shift
      ;;
    --dry-run)
      DRY=1
      shift
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

die() { echo "✗ $1" >&2; exit 2; }

[ -d "$RUST_SPECS_DIR/lints" ] || die "rust-specs 不在 $RUST_SPECS_DIR（RUST_SPECS_DIR 可覆盖）"
rustup toolchain list 2>/dev/null | grep -q "^$DYLINT_TOOLCHAIN" \
  || die "toolchain $DYLINT_TOOLCHAIN 未安装（需 rustc-dev 组件）"

# 变更文件（仓库相对路径，.rs 结尾）：基线 → HEAD，再并上工作区与暂存区。
changed_files() {
  {
    git diff --name-only "$BASE"...HEAD 2>/dev/null || true
    git diff --name-only HEAD 2>/dev/null || true
    git diff --name-only --cached 2>/dev/null || true
  } | sort -u | grep -E '\.rs$' || true
}

# dylint 拒绝一次带多个 -p；按 cargo metadata 把文件映射到所属包，逐包跑。
# 单包仓（silverq）只有一个包，映射退化成「全仓」。
package_of_file() {
  cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c '
import json, sys
meta = json.load(sys.stdin)
root = meta["workspace_root"]
for pkg in meta["packages"]:
    for target in pkg["targets"]:
        if target["kind"] and "test" in target["kind"]:
            continue
        prefix = pkg["manifest_path"].rsplit("/", 1)[0]
        for path in sys.argv[1:]:
            if path.startswith(prefix + "/"):
                print(pkg["name"])
                sys.exit(0)
' "$@"
}

LINT_LIB_DIR="$ROOT/target/dylint/libraries/$DYLINT_TOOLCHAIN-x86_64-unknown-linux-gnu/release"

# 补齐 plain 名 .so：dylint 编到 rust-specs 的 target/release/，metadata 步骤
# 却在消费仓的 dylint 目录里按 plain 名找。缺了就从 rust-specs 的产物拷过来。
stage_lint_libs() {
  mkdir -p "$LINT_LIB_DIR"
  local shopt_nullglob
  shopt_nullglob=1
  local produced=("$RUST_SPECS_DIR/target/release/"*.so)
  shopt -u nullglob
  [ ${#produced[@]} -gt 0 ] || return 0
  local so
  for so in "${produced[@]}"; do
    local base
    base="$(basename "$so")"
    [ -f "$LINT_LIB_DIR/$base" ] || cp -f "$so" "$LINT_LIB_DIR/$base"
  done
}

if [ "$ALL" = 1 ]; then
  pkgs=(".")
  files=""
else
  mapfile -t files < <(changed_files)
  if [ ${#files[@]} -eq 0 ]; then
    echo "PASS: 0 findings, 0 failures (无变更文件)"
    exit 0
  fi
  declare -A seen=()
  pkgs=()
  for f in "${files[@]}"; do
    p="$(package_of_file "$f")"
    if [ -n "$p" ] && [ -z "${seen[$p]:-}" ]; then
      seen[$p]=1
      pkgs+=("$p")
    fi
  done
  [ ${#pkgs[@]} -gt 0 ] || pkgs=(".")
fi

# 先把 lint 编出来（dylint 的 metadata 步骤需要产物就位）。
build_lints() {
  (cd "$RUST_SPECS_DIR" && cargo +"$DYLINT_TOOLCHAIN" build --release \
    --manifest-path "$RUST_SPECS_DIR/lints/ban-unwrap/Cargo.toml" \
    --target-dir "$RUST_SPECS_DIR/target" 2>/dev/null) || true
  # 全量编（一次覆盖所有 lint，共享依赖只编一次）
  (cd "$RUST_SPECS_DIR" && cargo +"$DYLINT_TOOLCHAIN" build --release \
    --workspace --exclude checks --exclude agents 2>/dev/null) || true
}

run_one() { # pkg
  local pkg="$1"
  local -a cmd=(cargo +"$DYLINT_TOOLCHAIN" dylint --path "$RUST_SPECS_DIR" --pattern 'lints/*' --all)
  if [ "$ALL" = 0 ]; then
    cmd+=(-p "$pkg" --no-deps)
  else
    cmd+=(--workspace)
  fi
  cmd+=(-- --message-format=short)

  if [ "$DRY" = 1 ]; then
    printf 'DRY:' >&2
    printf ' %q' "${cmd[@]}" >&2
    echo >&2
    return 0
  fi

  local out rc
  out="$(mktemp)"
  set +e
  "${cmd[@]}" >"$out" 2>&1
  rc=$?
  set -e
  # dylint 的 finding 走 cargo 的 warning 通道（退出码 0），只把 lint 诊断留下。
  # rust-specs 各 lint crate 自己的 manifest 警告（missing [lints] 等）与本仓无关，
  # 一并滤掉——它们是规则库的构建噪音，不是被扫仓的 finding。
  grep -E "^(warning|error)(\[|:)" "$out" \
    | grep -vE "generated [0-9]+ warning|unused (dependency|manifest)|missing \[lints\]|ignoring the configured target directory" \
    || true
  # 工具链坏了（找不到 metadata / 编译失败）不能当「零 finding」静默放行。
  if grep -qE "Could not get metadata|^error: could not compile|^error: Error" "$out"; then
    echo "dylint 工具链失败:" >&2
    grep -E "Could not get metadata|^error" "$out" | head -5 >&2
    rm -f "$out"
    return 2
  fi
  rm -f "$out"
  return 0
}

build_lints
stage_lint_libs

failures=0
for pkg in "${pkgs[@]}"; do
  if ! run_one "$pkg"; then
    failures=$((failures + 1))
  fi
done

if [ "$failures" -gt 0 ]; then
  echo "FAIL: dylint 工具链失败（$failures 个包）" >&2
  exit 2
fi
if [ "$FAIL_ON_FINDINGS" = 1 ]; then
  echo "dylint 扫描完成（finding 见上）"
fi
exit 0
