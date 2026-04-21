#!/usr/bin/env bash
# Mock: exercises the --session-mirror path. Emits init, responds to
# initialize, reads one user turn, then emits two system/transcript_mirror
# frames, finishes with an assistant turn + result.

set -euo pipefail

# --version: print a synthetic version.
if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-mirror-001","cwd":"/tmp","tools":["Edit"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

# Read + respond to initialize control_request.
IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"

# Read one user message.
IFS= read -r _user

# Emit two transcript_mirror frames: one user entry, one assistant entry.
printf '%s\n' '{"type":"system","subtype":"transcript_mirror","session_id":"mock-mirror-001","project_key":"proj_mock","subpath":null,"entries":[{"type":"user","content":"hi"}]}'
printf '%s\n' '{"type":"system","subtype":"transcript_mirror","session_id":"mock-mirror-001","project_key":"proj_mock","subpath":null,"entries":[{"type":"assistant","content":"hello"}]}'

# Emit the turn's assistant reply + result so next_event surfaces
# something to the test and disconnect exits cleanly.
printf '%s\n' '{"type":"assistant","message":{"id":"msg_mirror","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-mirror-001","parent_tool_use_id":null}'
printf '%s\n' '{"type":"result","subtype":"success","duration_ms":20,"duration_api_ms":15,"is_error":false,"num_turns":1,"session_id":"mock-mirror-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
