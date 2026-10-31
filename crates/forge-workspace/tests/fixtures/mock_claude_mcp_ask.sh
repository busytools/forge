#!/usr/bin/env bash
# Mock of `claude --output-format stream-json --input-format stream-json`.
#
# Emits init, answers initialize, reads one user message, then asks the
# in-process `forge` server for `documents__hand_off` over the wire's
# `mcp_message` control_request. It reads the answer before finishing the
# turn, so a tool call that waited on the person would hang this script and
# the turn would never complete - which is what the test's timeout catches.
# Used exclusively by forge-workspace's MCP boundary test.

set -euo pipefail

if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.280 (anthropic-mock-ask)"
    exit 0
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-ask-001","cwd":"/tmp","tools":["Read"],"mcp_servers":[{"name":"forge","status":"connected"}],"model":"claude-opus-4-5","permissionMode":"bypassPermissions","apiKeySource":"ANTHROPIC_API_KEY"}'

IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; print(json.load(sys.stdin).get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"

# One user message starts the turn.
IFS= read -r _user

printf '%s\n' '{"type":"assistant","message":{"id":"msg_mock_ask_1","role":"assistant","model":"claude-opus-4-5","content":[{"type":"tool_use","id":"toolu_ask_1","name":"mcp__forge__documents__hand_off","input":{"path":"docs/readme.md"}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-ask-001","parent_tool_use_id":null}'

printf '%s\n' '{"type":"control_request","request_id":"req_mcp_ask_1","request":{"subtype":"mcp_message","server_name":"forge","message":{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"documents__hand_off","arguments":{"path":"docs/readme.md"}}}}}'

# A tool call that waits on the person never answers here, so the turn's
# result never follows and the test times out.
IFS= read -r mcp_response
printf '%s\n' "MOCK_MCP_RESPONSE: $mcp_response" 1>&2

printf '%s\n' '{"type":"assistant","message":{"id":"msg_mock_ask_2","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"the hand-off was handed"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-ask-001","parent_tool_use_id":null}'
printf '%s\n' '{"type":"result","subtype":"success","duration_ms":50,"duration_api_ms":40,"is_error":false,"num_turns":1,"session_id":"mock-ask-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
