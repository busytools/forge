#!/usr/bin/env bash
# Mock: captures the initialize control_request body to a file whose
# path is in $FORGE_TEST_INIT_CAPTURE. Used by initialize_payload tests
# to verify the wire shape forge-sdk sends matches Python's conditional
# field-inclusion rules.

set -euo pipefail

if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-init-001","cwd":"/tmp","tools":[],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

IFS= read -r init_req
if [[ -n "${FORGE_TEST_INIT_CAPTURE:-}" ]]; then
    printf '%s' "$init_req" > "$FORGE_TEST_INIT_CAPTURE"
fi
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; print(json.load(sys.stdin).get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"
