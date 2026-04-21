#!/usr/bin/env bash
# Mock: emits init, on first user input emits a permission request,
# reads the response, then emits an assistant turn + result.
#
# The allow-branch reply echoes `updatedInput.file_path` so tests can
# verify path propagation through the permission wire.

set -euo pipefail

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-perm-001","cwd":"/tmp","tools":["Edit"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

# Read one user message.
IFS= read -r _user

# Emit a permission request.
printf '%s\n' '{"type":"control_request","request_id":"req_fixture_01","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"/tmp/x.txt","old_string":"a","new_string":"b"},"permission_suggestions":[],"blocked_path":null,"tool_use_id":"toolu_fixture_01","agent_id":null}}'

# Read the permission response.
IFS= read -r perm_response

# Echo what we got for test introspection (goes to stderr, not stream-json).
printf '%s\n' "MOCK_RECEIVED_PERMISSION: $perm_response" 1>&2

# Extract updatedInput for observability.
if printf '%s' "$perm_response" | grep -q '"updatedInput"'; then
    updated=$(printf '%s' "$perm_response" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(json.dumps(d.get("response",{}).get("response",{}).get("updatedInput")))' 2>/dev/null || echo "null")
    printf '%s\n' "MOCK_UPDATED_INPUT: $updated" 1>&2
fi

# Branch on whether it was allow or deny.
if printf '%s' "$perm_response" | grep -q '"behavior":"allow"'; then
    # Allow-branch: surface the file_path from updatedInput in the reply so
    # integration tests can assert propagation.
    path=$(printf '%s' "$perm_response" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("response",{}).get("response",{}).get("updatedInput",{}).get("file_path","/tmp/x.txt"))' 2>/dev/null || echo "/tmp/x.txt")
    printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_perm_ok\",\"role\":\"assistant\",\"model\":\"claude-opus-4-5\",\"content\":[{\"type\":\"text\",\"text\":\"edited $path\"}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}},\"session_id\":\"mock-perm-001\",\"parent_tool_use_id\":null}"
else
    printf '%s\n' '{"type":"assistant","message":{"id":"msg_perm_no","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"edit denied"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-perm-001","parent_tool_use_id":null}'
fi

printf '%s\n' '{"type":"result","subtype":"success","duration_ms":50,"duration_api_ms":40,"is_error":false,"num_turns":1,"session_id":"mock-perm-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
