#!/usr/bin/env bash
# Mock: dumps the environment it was spawned with to a file whose path
# it reads from $FORGE_TEST_ENV_DUMP. Used by transport_env tests to
# verify Client::spawn injects the right env (CLAUDE_CODE_ENTRYPOINT,
# CLAUDE_AGENT_SDK_VERSION, PWD, CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING).

set -euo pipefail

if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

if [[ -n "${FORGE_TEST_ENV_DUMP:-}" ]]; then
    env > "$FORGE_TEST_ENV_DUMP"
fi

# Init message then initialize handshake, same shape as mock_claude_control.
printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-env-001","cwd":"/tmp","tools":[],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'

IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; print(json.load(sys.stdin).get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"

# Exit - caller disconnects immediately.
