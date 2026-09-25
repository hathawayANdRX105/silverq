#!/usr/bin/env bash
# mock harness for testing checklist pipeline without real LLM
# 用法: 在 demo yaml 的 harness.command 改成 "./.githooks/spec/CHECKLIST_DEMO_MOCK.sh"
#       env CHECKLIST_DEMO_FINDINGS 传入预制的 finding JSON
#
# 测试三种分支:
#   - 默认:  echo $CHECKLIST_DEMO_FINDINGS (默认 [])
#   - BROKEN: echo 非 JSON, 验证 gate 容错
#   - FAIL:   exit 2, 验证 harness 自身报错
out="${CHECKLIST_DEMO_FINDINGS:-[]}"
if [ "${CHECKLIST_DEMO_BROKEN:-0}" = "1" ]; then
  echo "not valid json {"
  exit 0
fi
if [ "${CHECKLIST_DEMO_FAIL:-0}" = "1" ]; then
  echo "harness internal error" >&2
  exit 2
fi
echo "$out"
exit 0
