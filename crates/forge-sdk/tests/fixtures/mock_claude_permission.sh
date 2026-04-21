#!/usr/bin/env bash
# Mock: emits init, responds to initialize, reads user prompt, emits
# permission request, reads response, emits assistant + result.

set -euo pipefail

# --version: print a synthetic version.
if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-perm-001","cwd":"/tmp","tools":["Edit"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

# Read + respond to initialize control_request.
IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"

# Read one user message.
IFS= read -r _user

# Emit a permission request.
printf '%s\n' '{"type":"control_request","request_id":"req_fixture_01","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"/tmp/x.txt","old_string":"a","new_string":"b"},"permission_suggestions":[],"blocked_path":null,"tool_use_id":"toolu_fixture_01","agent_id":null}}'

IFS= read -r perm_response
printf '%s\n' "MOCK_RECEIVED_PERMISSION: $perm_response" 1>&2

if printf '%s' "$perm_response" | grep -q '"updatedInput"'; then
    updated=$(printf '%s' "$perm_response" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(json.dumps(d.get("response",{}).get("response",{}).get("updatedInput")))' 2>/dev/null || echo "null")
    printf '%s\n' "MOCK_UPDATED_INPUT: $updated" 1>&2
fi

if printf '%s' "$perm_response" | grep -q '"behavior":"allow"'; then
    path=$(printf '%s' "$perm_response" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("response",{}).get("response",{}).get("updatedInput",{}).get("file_path","/tmp/x.txt"))' 2>/dev/null || echo "/tmp/x.txt")
    printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_perm_ok\",\"role\":\"assistant\",\"model\":\"claude-opus-4-5\",\"content\":[{\"type\":\"text\",\"text\":\"edited $path\"}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}},\"session_id\":\"mock-perm-001\",\"parent_tool_use_id\":null}"
else
    printf '%s\n' '{"type":"assistant","message":{"id":"msg_perm_no","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"edit denied"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-perm-001","parent_tool_use_id":null}'
fi

printf '%s\n' '{"type":"result","subtype":"success","duration_ms":50,"duration_api_ms":40,"is_error":false,"num_turns":1,"session_id":"mock-perm-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
