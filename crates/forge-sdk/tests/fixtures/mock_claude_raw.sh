#!/usr/bin/env bash
# Raw mock (no initialize handshake) — for Subprocess-level transport tests
# that bypass Client::spawn. Emits init, then per stdin line emits
# assistant+result.

set -euo pipefail

# --version: print a synthetic version.
if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-session-001","cwd":"/tmp","tools":["Edit","Read"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

while IFS= read -r _line; do
    printf '%s\n' '{"type":"assistant","message":{"id":"msg_mock_01","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"hello from mock"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-session-001","parent_tool_use_id":null}'
    printf '%s\n' '{"type":"result","subtype":"success","duration_ms":42,"duration_api_ms":30,"is_error":false,"num_turns":1,"session_id":"mock-session-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
done
