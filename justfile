# silverq — justfile
#
# 门禁与日常命令的入口。规范正本在 rust-specs 仓（`~/projects/rust-specs`）——
# `checks` crate 管仓库结构规则与 GitHub PR/issue 格式，`lints/` 是 dylint 语法规则。
#
# AGENTS.md 的「本地重命令禁令」仍然有效：just 的 target 不会替你绕过它。
# 编译 / 测试 / 装包这类 CPU 密集命令仍要 `.githooks/heavy` 包裹。

# rust-specs 检出目录（规则库）。与 .cargo/config.toml 的 RUST_SPECS_DIR 同源。
rust_specs_dir := env_var_or_default("RUST_SPECS_DIR", "/home/hathaway/projects/rust-specs")

default: check

# Build check
check:
    cargo check --features meow-tun --all-targets

# Format
fmt:
    cargo fmt --all

# Format check (no file changes)
fmt-check:
    cargo fmt --all --check

# Clippy
clippy:
    cargo clippy --all-targets -- -D warnings

# Build binary
build:
    cargo build

# Run the daemon against the production config
run:
    silverq serve ~/.config/silverq/nodes.yaml --config ~/.config/silverq/silverq.toml

# ── 规范门禁（rust-specs）────────────────────────────────────────────────

# Normative gate — structure rules. Usage:
#   just spec-check                          # changed scope (default)
#   just spec-check --scope all              # full repo
#   just spec-check --class advisory         # advisory layer
#   just spec-check --class semantic         # jev semantic layer (needs the jev endpoint)
#   just spec-check --base <ref>             # custom base
spec-check *args:
    bash scripts/spec-check.sh {{args}}

# dylint syntax lint (rust-specs general rules). Usage:
#   just lint                                # changed scope
#   just lint --all                          # full repo
#   just lint --base <ref>                   # custom base
lint *args:
    bash scripts/lint-dylint.sh {{args}}

# Both gate layers: structure rules + dylint. The pre-push hook runs the
# structure layer only; this is the full local check.
gate: spec-check lint

# GitHub PR format gate (rust-specs checks crate). Usage:
#   just pr-check <number>
#   just pr-check --title "..." --body-file <path> [--labels a,b] [--branch <name>]
pr-check *args:
    cargo xtask check github-pr {{args}}

# GitHub issue format gate (rust-specs checks crate).
issue-check *args:
    cargo xtask check github-issue {{args}}

# AGENTS.md drift check — the file is generated from the rust-specs profile.
agents-check:
    cargo run -q --manifest-path {{rust_specs_dir}}/agents/Cargo.toml -p agents -- check silverq

# Render AGENTS.md from the rust-specs profile (writes the repo's AGENTS.md).
agents-push:
    cargo run -q --manifest-path {{rust_specs_dir}}/agents/Cargo.toml -p agents -- push silverq --force
