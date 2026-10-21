#!/usr/bin/env bash
# Mock: answers the initialize handshake, leaves a background
# descendant behind, then exits 0 on stdin EOF. Writes the
# descendant's pid to $FORGE_TEST_ORPHAN_MARKER, so a test asserts on
# the pid rather than on a process-name pattern.
set -euo pipefail

if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

: "${FORGE_TEST_ORPHAN_MARKER:?the test must name its pid marker}"

# A signal aimed at forge's own group reaches this child directly only if
# the child shares that group; the marker says it did. The fixture goes on
# serving, so a harness can read both answers.
if [ -n "${FORGE_TEST_CHILD_TERM_MARKER:-}" ]; then
    trap 'printf x > "$FORGE_TEST_CHILD_TERM_MARKER"' TERM
fi

printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-session-001","cwd":"/tmp","tools":["Edit","Read"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

IFS= read -r init_req
req_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$req_id\",\"response\":{}}}"

# Job control is off in a non-interactive shell, so this sleeper stays
# in the script's own process group. Its stdio is redirected so a
# surviving descendant reads as a live pid, not as a teardown parked on
# a stderr that never reaches EOF. TERM is ignored, so only the hard
# kill can end it - the signal teardown sends.
( trap '' TERM; exec sleep 300 ) </dev/null >/dev/null 2>&1 &
printf '%s\n' "$!" > "$FORGE_TEST_ORPHAN_MARKER"

# Stdin EOF is the graceful exit a teardown asks for. A test that needs a
# session still closing while another has finished sets a delay.
while IFS= read -r _line; do :; done
if [ -n "${FORGE_TEST_CLOSE_DELAY:-}" ]; then
    sleep "$FORGE_TEST_CLOSE_DELAY"
fi
