#!/usr/bin/env bash
# scripts/lint-dylint.sh — rust-specs dylint 语法 lint（通用规则），按 git diff 收窄。
#
# 用法（日常入口是 justfile 的 `just lint`，本脚本是它的实现）：
#   scripts/lint-dylint.sh                   # changed：基线到 HEAD + 工作区/暂存
#   scripts/lint-dylint.sh --base <ref>      # 指定基线（默认 origin/master）
#   scripts/lint-dylint.sh --all             # 全 workspace（不按 diff 过滤）
#
# 环境变量:
#   DYLINT_TOOLCHAIN dylint 用的 toolchain（默认 nightly-2026-08-20，须带 rustc-dev）
#
# 退出码:
#   0 = 无 finding
#   2 = 工具链 / rust-specs / 基线缺失
#
# 规则库接入方式（2026-11-21 起）: [workspace.metadata.dylint] git-tag 条目
# （root Cargo.toml，tag 固定 lints-v2026.11.21）。`cargo dylint --all` 由 dylint
# 自己拉取、编译并加载 lints/*，本脚本不再从 RUST_SPECS_DIR 预编译拷贝 .so。
# 保留本脚本只做两件事：按 git diff 收窄扫描范围（cargo dylint 没有 changed-
# scope 概念），以及滤掉规则库自身的 manifest 噪音。升级规则 = 改 Cargo.toml
# 里的 tag（见 ~/homono/specs/rust-specs-projects.md §6）。
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "✗ 不在 git 仓库内" >&2; exit 2; }
cd "$ROOT"

DYLINT_TOOLCHAIN="${DYLINT_TOOLCHAIN:-nightly-2026-08-20}"
BASE="origin/master"
ALL=0

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
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

die() { echo "✗ $1" >&2; exit 2; }

rustup toolchain list 2>/dev/null | grep -q "^$DYLINT_TOOLCHAIN" \
  || die "toolchain $DYLINT_TOOLCHAIN 未安装（需 rustc-dev 组件）"

# 变更文件（仓库相对路径，.rs 结尾）：基线 → HEAD，再并上工作区与暂存区。
mapfile -t files < <(
  {
    git diff --name-only "$BASE"...HEAD 2>/dev/null || true
    git diff --name-only HEAD 2>/dev/null || true
    git diff --name-only --cached 2>/dev/null || true
  } | sort -u | grep -E '\.rs$' || true
)

if [ "$ALL" = 0 ] && [ ${#files[@]} -eq 0 ]; then
  echo "PASS: 0 findings, 0 failures (无变更文件)"
  exit 0
fi

# dylint 拒绝一次带多个 -p；按 cargo metadata 把文件映射到所属包，逐包跑。
# 单包仓（silverq）只有一个包，映射退化成「全仓」。
package_of_file() {
  cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c '
import json, sys
meta = json.load(sys.stdin)
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

if [ "$ALL" = 1 ]; then
  pkgs=(".")
else
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

for pkg in "${pkgs[@]}"; do
  # metadata entry 加载（dylint 自己从 tag 拉取并编译规则库）。
  cmd=(cargo +"$DYLINT_TOOLCHAIN" dylint --all)
  if [ "$ALL" = 0 ]; then
    cmd+=(-p "$pkg" --no-deps)
  else
    cmd+=(--workspace)
  fi
  cmd+=(-- --message-format=short)

  out="$(mktemp)"
  set +e
  "${cmd[@]}" >"$out" 2>&1
  set -e

  # dylint 的 finding 走 cargo 的 warning 通道（退出码 0），只把 lint 诊断留下。
  # --message-format=short 的行首是 "path:line:col: warning: ..."，不能锚定行首；
  # rust-specs 各 lint crate 自己的 manifest 警告（missing [lints] 等）与本仓
  # 无关，一并滤掉——它们是规则库的构建噪音，不是被扫仓的 finding。
  grep -E "(^|: )(warning|error)(\[|:)" "$out" \
    | grep -vE "generated [0-9]+ warning|unused (dependency|manifest)|missing \[lints\]|ignoring the configured target directory|workspace \(manifest\) generated" \
    || true

  # 工具链坏了（找不到 metadata / 编译失败）不能当「零 finding」静默放行。
  if grep -qE "Could not get metadata|^error: could not compile|^error: Error" "$out"; then
    echo "dylint 工具链失败:" >&2
    grep -E "Could not get metadata|^error" "$out" | head -5 >&2
    rm -f "$out"
    exit 2
  fi
  rm -f "$out"
done
