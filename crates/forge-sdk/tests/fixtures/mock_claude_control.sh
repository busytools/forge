#!/usr/bin/env bash
# Mock: handles the initialize handshake, then loops reading one
# outbound control_request at a time and replies with a matching
# control_response. Used exclusively by the control-subtype
# integration tests - never emits user-message or result frames.
#
# When `FORGED_MOCK_ECHO_SUBTYPE` is set, every
# observed control_request's subtype is appended (one per line) to
# the file at `$FORGED_MOCK_ECHO_SUBTYPE`. M3 dispatch tests can read
# the file post-call to discriminate "the right Client::* method
# fired" from "the dispatch path reached the actor" - strengthening
# the dispatch contract beyond the current "no SessionNotFound, no
# actor-gone" check. Default (env unset) keeps the mock byte-for-byte
# compatible with existing forge-sdk tests.

set -euo pipefail

# --version: print a synthetic version.
if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

# 1. Init message (stream-json `system`/`init`).
printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-ctrl-001","cwd":"/tmp","tools":["Edit","Read"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

# 2. Read initialize control_request and respond with a canned server-info
#    body (commands + outputStyle) so Client::get_server_info exposes it.
IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{\"commands\":[{\"name\":\"/help\"}],\"outputStyle\":\"default\"}}}"

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

    # Optional: echo the observed subtype so dispatch tests can
    # discriminate between code paths. Gated on env var so the mock
    # stays byte-for-byte compatible with existing forge-sdk tests
    # that don't set this.
    if [[ -n "${FORGED_MOCK_ECHO_SUBTYPE:-}" ]]; then
        printf '%s\n' "$subtype" >> "$FORGED_MOCK_ECHO_SUBTYPE"
    fi

    # Optional: never answer the named subtype so tests can drive the
    # response-timeout path. Gated on env var; unset keeps the mock
    # byte-for-byte compatible with existing forge-sdk tests.
    if [[ -n "${FORGED_MOCK_SKIP_SUBTYPE:-}" && "$subtype" == "$FORGED_MOCK_SKIP_SUBTYPE" ]]; then
        continue
    fi

    case "$subtype" in
        mcp_status)
            response_payload='{"mcpServers":[]}'
            ;;
        get_context_usage)
            response_payload='{"categories":[],"totalTokens":0,"maxTokens":200000,"rawMaxTokens":200000,"percentage":0,"model":"claude-opus-4-5","isAutoCompactEnabled":false,"memoryFiles":[],"mcpTools":[],"agents":[],"gridRows":[]}'
            ;;
        *)
            response_payload='{}'
            ;;
    esac

    printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$req_id\",\"response\":$response_payload}}"
done
