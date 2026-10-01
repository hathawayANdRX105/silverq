# canon 体系：性能与测试配方（详见 rust-dev-perf skill）
default:
    @just --list

# 全量测试（--workspace；根包 workspace 下裸 cargo test 只跑根包）
test:
    cargo test --workspace

# ── canon 体系：testless 函数级测试选择（rust-dev-perf skill）──
# 开发内环：只跑本次改动可能破坏的测试；三重降级保险（testless 异常/
# JSON 异常/零命中 → 全量 workspace），绝不静默跳过。
# 用法：just test-fast（对比 HEAD）/ just test-fast main
test-fast base="HEAD":
    #!/usr/bin/env bash
    set -uo pipefail
    json="$(testless select --from "{{base}}" 2>/dev/null)" && rc=0 || rc=$?
    if [ "$rc" -ne 0 ] || ! printf '%s' "$json" | jq -e . >/dev/null 2>&1; then
        echo "testless 不可用或输出异常（exit=$rc）→ 降级全量（workspace）"
        exec cargo test --workspace
    fi
    names="$(printf '%s\n' "$json" | jq -r '.tests[] | .name | last' | sort -u | tr '\n' ' ')"
    if [ -z "${names// }" ]; then
        echo "✓ 本次改动（vs {{base}}）不影响任何测试"
        exit 0
    fi
    echo "受影响测试过滤器: $names"
    out="$(cargo test --workspace -- $names 2>&1)" && rc=0 || rc=$?
    printf '%s\n' "$out"
    passed="$(printf '%s\n' "$out" | awk '/^test result:/{for(i=1;i<=NF;i++) if($i=="passed;") s+=$(i-1)} END{print s+0}')"
    if [ "$rc" -ne 0 ]; then exit 1; fi
    if [ "$passed" -eq 0 ]; then
        echo "过滤器零命中（可疑）→ 降级全量（workspace）"
        exec cargo test --workspace
    fi
    echo "✓ 受影响测试全部通过（passed=$passed）"
