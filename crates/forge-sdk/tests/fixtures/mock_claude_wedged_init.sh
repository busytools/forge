#!/usr/bin/env bash
# Mock: spawns but never answers the initialize control_request -
# the wedged-CLI case where the process is alive and stdout stays
# open but the init handshake never completes. Drives the
# Client::spawn init-timeout test; never emits a frame.
set -euo pipefail

if [[ ${1:-} == "--version" ]]; then
    printf "%s\n" "2.1.116 (anthropic-mock)"
    exit 0
fi

exec sleep 300
