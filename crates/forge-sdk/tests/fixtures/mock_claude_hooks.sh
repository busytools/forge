#!/usr/bin/env bash
# Mock: emits init, responds to initialize, reads user prompt, emits
# PreToolUse hook request, reads hook response, emits assistant + result.

set -euo pipefail

# --version: print a synthetic version.
if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-hooks-001","cwd":"/tmp","tools":["Bash"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"

IFS= read -r _user

printf '%s\n' '{"type":"control_request","request_id":"req_hook_01","request":{"subtype":"hook_callback","callback_id":"hook_0","input":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo original"},"session_id":"mock-hooks-001","transcript_path":"/tmp/t","cwd":"/tmp","permission_mode":"default","tool_use_id":"toolu_hook_01"},"tool_use_id":"toolu_hook_01"}}'

IFS= read -r hook_response
printf '%s\n' "MOCK_HOOK_RESP: $hook_response" 1>&2

cmd=$(printf '%s' "$hook_response" | python3 -c '
import sys, json
d = json.load(sys.stdin)
hso = d.get("response", {}).get("response", {}).get("hookSpecificOutput", {})
ui = hso.get("updatedInput", {})
ti = ui.get("tool_input", {}) if isinstance(ui, dict) else {}
print(ti.get("command", "echo original"))
' 2>/dev/null || echo "echo original")

if printf '%s' "$hook_response" | grep -q '"decision":"block"'; then
    printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_hook_deny\",\"role\":\"assistant\",\"model\":\"claude-opus-4-5\",\"content\":[{\"type\":\"text\",\"text\":\"hook denied\"}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}},\"session_id\":\"mock-hooks-001\",\"parent_tool_use_id\":null}"
else
    printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_hook\",\"role\":\"assistant\",\"model\":\"claude-opus-4-5\",\"content\":[{\"type\":\"text\",\"text\":\"ran: $cmd\"}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}},\"session_id\":\"mock-hooks-001\",\"parent_tool_use_id\":null}"
fi

printf '%s\n' '{"type":"result","subtype":"success","duration_ms":20,"duration_api_ms":15,"is_error":false,"num_turns":1,"session_id":"mock-hooks-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
