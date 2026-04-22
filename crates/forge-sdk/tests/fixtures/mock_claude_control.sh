#!/usr/bin/env bash
# Mock: handles the initialize handshake, then loops reading one
# outbound control_request at a time and replies with a matching
# control_response. Used exclusively by the control-subtype
# integration tests — never emits user-message or result frames.

set -euo pipefail

# --version: print a synthetic version.
if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

# 1. Init message (stream-json `system`/`init`).
printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-ctrl-001","cwd":"/tmp","tools":["Edit","Read"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

# 2. Read initialize control_request and respond.
IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"

# 3. Loop: read one outbound control_request line, classify by subtype,
# write back a matching control_response carrying a canned payload.
while IFS= read -r ctrl_req; do
    parsed=$(printf '%s' "$ctrl_req" | python3 -c '
import sys, json
d = json.load(sys.stdin)
req = d.get("request", {}) if isinstance(d.get("request"), dict) else {}
print(json.dumps({
    "request_id": d.get("request_id", ""),
    "subtype": req.get("subtype", ""),
}))
' 2>/dev/null || echo '{"request_id":"","subtype":""}')
    req_id=$(printf '%s' "$parsed" | python3 -c 'import sys, json; print(json.load(sys.stdin).get("request_id",""))' 2>/dev/null || echo "")
    subtype=$(printf '%s' "$parsed" | python3 -c 'import sys, json; print(json.load(sys.stdin).get("subtype",""))' 2>/dev/null || echo "")

    case "$subtype" in
        mcp_status)
            response_payload='{"servers":[]}'
            ;;
        get_context_usage)
            response_payload='{"used":0,"budget":200000}'
            ;;
        *)
            response_payload='{}'
            ;;
    esac

    printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$req_id\",\"response\":$response_payload}}"
done
