#!/usr/bin/env bash
# Mock: exercises the --session-mirror path. Emits init, responds to
# initialize, reads one user turn, then emits two TOP-LEVEL
# transcript_mirror frames, finishes with an assistant turn + result.
#
# Wire shape matches Python SDK v0.1.64
# `_internal/transcript_mirror_batcher.py:3` and `_internal/query.py:282-289`:
# `{"type":"transcript_mirror","filePath":"...","entries":[...]}` at the
# top level (NOT nested inside system/subtype).

set -euo pipefail

if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-mirror-001","cwd":"/tmp","tools":["Edit"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"

IFS= read -r _user

# Two top-level transcript_mirror frames — the SDK's file_path_to_session_key
# parses the filePath to derive (project_key, session_id, subpath?). The
# frame below places the transcript under <projects>/proj_mock/mock-mirror-001.jsonl.
printf '%s\n' '{"type":"transcript_mirror","filePath":"/tmp/forge-mirror-root/proj_mock/mock-mirror-001.jsonl","entries":[{"type":"user","content":"hi"}]}'
printf '%s\n' '{"type":"transcript_mirror","filePath":"/tmp/forge-mirror-root/proj_mock/mock-mirror-001.jsonl","entries":[{"type":"assistant","content":"hello"}]}'

printf '%s\n' '{"type":"assistant","message":{"id":"msg_mirror","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-mirror-001","parent_tool_use_id":null}'
printf '%s\n' '{"type":"result","subtype":"success","duration_ms":20,"duration_api_ms":15,"is_error":false,"num_turns":1,"session_id":"mock-mirror-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
