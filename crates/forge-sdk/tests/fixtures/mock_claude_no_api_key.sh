#!/usr/bin/env bash
# Minimal mock of `claude --output-format stream-json --input-format stream-json --verbose`
# whose init frame omits `apiKeySource`, so `Client::account_info_from_init`
# answers None - for forge-agent bridge tests that must exercise the
# status snapshot's identity-fallback arm.

set -euo pipefail

# --version: print a synthetic version.
if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

# 1. Init message (stream-json `system`/`init`), no apiKeySource.
printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-session-001","cwd":"/tmp","tools":["Edit","Read"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default"}'

# 2. Read initialize control_request and respond.
IFS= read -r init_req
req_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$req_id\",\"response\":{}}}"

# 3. Per input line, emit an assistant turn + result.
while IFS= read -r _line; do
    printf '%s\n' '{"type":"assistant","message":{"id":"msg_mock_01","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"hello from mock"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-session-001","parent_tool_use_id":null}'
    printf '%s\n' '{"type":"result","subtype":"success","duration_ms":42,"duration_api_ms":30,"is_error":false,"num_turns":1,"session_id":"mock-session-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
done
