#!/usr/bin/env bash
# llm-checklist-harness.sh — drop-in harness for gate checklist yamls.
#
# Reads the diff/file content from stdin, sends to an OpenAI-compatible
# chat completions endpoint, asks the model to emit a JSON array of
# findings matching gate's protocol:
#   [{id, severity, path?, line?, message}, ...] or []
#
# Configuration (env vars, fall back to safe defaults):
#   LLM_BASE_URL   — OpenAI-compatible base, e.g. http://127.0.0.1:3182/v1
#   LLM_API_KEY    — bearer token (any *_API_KEY env)
#   LLM_MODEL      — model id (default: auto-detect first from /v1/models)
#   LLM_TIMEOUT    — curl timeout seconds (default: 30)
#   LLM_PROMPT     — extra system instructions
#   LLM_DRY_RUN    — if "1", skip the API call and emit MOCK_FINDINGS
#   MOCK_FINDINGS  — JSON array to emit in dry-run mode
#
# Usage in a checklist yaml:
#   harness:
#     command: "sh"
#     args: ["-c", "cat | $REPO/.githooks/spec/llm-checklist-harness.sh"]

set -u

base="${LLM_BASE_URL:-${NEWAPI_HEALTH_SIM_BASE_URL:-}}"
key="${LLM_API_KEY:-${CC_API_KEY:-${GLM_API_KEY:-${ANTHROPIC_API_KEY:-${AGNES_API_KEY:-}}}}}}"
model="${LLM_MODEL:-}"
timeout="${LLM_TIMEOUT:-30}"
extra_prompt="${LLM_PROMPT:-}"

if [ "${LLM_DRY_RUN:-0}" = "1" ]; then
  echo "${MOCK_FINDINGS:-[]}"
  exit 0
fi

if [ -z "$base" ] || [ -z "$key" ]; then
  echo "llm-harness: missing LLM_BASE_URL or LLM_API_KEY" >&2
  exit 2
fi

for bin in curl jq; do
  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "llm-harness: missing dependency: $bin" >&2
    exit 127
  fi
done

if [ -z "$model" ]; then
  model=$(curl -sS -m "$timeout" -H "Authorization: Bearer $key" \
    "$base/models" 2>/dev/null \
    | jq -r '.data[0].id // empty' 2>/dev/null)
  if [ -z "$model" ]; then
    echo "llm-harness: LLM_MODEL unset and /v1/models returned nothing" >&2
    exit 2
  fi
fi

content=$(head -c 102400)

system_msg="You are a code-quality reviewer. ${extra_prompt}
Given the provided diff or file content, output STRICTLY a JSON array of findings.
Each element: {\"id\": \"<rule-id>\", \"severity\": \"FAIL|WARN|INFO\", \"path\": \"<optional relative path>\", \"line\": <optional int>, \"message\": \"<one-line reason>\"}.
If nothing matches, output [].
Do NOT output anything outside the JSON array. No markdown, no explanation, no leading/trailing text."

user_msg="Input to review:
\`\`\`
${content}
\`\`\`"

req_body=$(jq -n \
  --arg model "$model" \
  --arg sys "$system_msg" \
  --arg usr "$user_msg" \
  '{model: $model, messages: [{role: "system", content: $sys}, {role: "user", content: $usr}], temperature: 0}')

response=$(curl -sS -m "$timeout" \
  -H "Authorization: Bearer $key" \
  -H "Content-Type: application/json" \
  -d "$req_body" \
  "$base/chat/completions" 2>&1)
rc=$?

if [ $rc -ne 0 ]; then
  echo "llm-harness: curl failed (rc=$rc): $response" >&2
  exit 127
fi

content_out=$(echo "$response" | jq -r '.choices[0].message.content // empty' 2>/dev/null)
if [ -z "$content_out" ]; then
  echo "llm-harness: empty/unexpected response: $(echo "$response" | head -c 200)" >&2
  exit 2
fi

content_out=$(echo "$content_out" | sed -e 's/^```json[[:space:]]*//' -e 's/^```[[:space:]]*//' -e 's/```[[:space:]]*$//' | tr -d '\r')

if echo "$content_out" | jq -e 'type == "array"' >/dev/null 2>&1; then
  echo "$content_out"
  exit 0
fi

last_arr=$(echo "$content_out" | grep -oE '\[[^]]*\]' | tail -1)
if [ -n "$last_arr" ] && echo "$last_arr" | jq -e 'type == "array"' >/dev/null 2>&1; then
  echo "$last_arr"
  exit 0
fi

echo "llm-harness: model output not a JSON array: $(echo "$content_out" | head -c 200)" >&2
exit 2
