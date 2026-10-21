#!/usr/bin/env bash
# Mock: leaves a background descendant behind, then ignores stdin EOF
# so teardown reaches the SIGKILL path with the descendant still
# running. Writes the descendant's pid to $FORGE_TEST_ORPHAN_MARKER, so
# a test asserts on the pid rather than on a process-name pattern.
set -euo pipefail

if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

: "${FORGE_TEST_ORPHAN_MARKER:?the test must name its pid marker}"

# Job control is off in a non-interactive shell, so this sleeper stays
# in the script's own process group. Its stdio is redirected so the
# test sees a surviving descendant as a live pid; inheriting the
# child's pipes would instead park teardown on a stderr that never
# reaches EOF, which is a second symptom of the same leak.
sleep 300 </dev/null >/dev/null 2>&1 &
printf '%s\n' "$!" > "$FORGE_TEST_ORPHAN_MARKER"

# Replace the shell so stdin EOF cannot end the child: only a signal can.
exec sleep 300
