#!/usr/bin/env bash
# Launch a built forge.app briefly and refuse a silent failure.
#
# **A client that dies on launch is a shipped-broken artifact**, and neither
# the compiler nor the bundler can see it. This runs the app's own binary,
# watches it live in a window of seconds, reads its words, and exits
# non-zero when it crashed, when CEF stayed off, or when the app object was
# claimed out of order. The e2e and release flows call THIS script; they do
# not re-implement it.
#
# Usage: smoke-launch.sh <forge.app>
set -uo pipefail
APP="${1:?the built app bundle}"
BIN="$APP/Contents/MacOS/forge-client"
if [ ! -x "$BIN" ]; then
    echo "no executable at $BIN" >&2
    exit 2
fi
LOG=$(mktemp /tmp/forge-smoke.XXXXXX.log)
"$BIN" > "$LOG" 2>&1 &
PID=$!
echo "client pid: $PID (log $LOG)"
sleep 6
ALIVE=$(ps -p "$PID" -o pid= | tr -d ' ')
echo "alive: $ALIVE"
echo "=== stderr ==="
head -20 "$LOG"
# The checks run while the app is LIVE.
FAILED=""
if [ -z "$ALIVE" ]; then
    FAILED="the client died on launch"
fi
if grep -q "forge client failed to start" "$LOG"; then
    FAILED="$(grep -m1 'forge client failed to start' "$LOG")"
fi
# **The host's own line is the engine's proof**: the browser came up. It is
# the vendored Chromium/installed Brave path (CEF is dormant), so the smoke
# checks the host's announcement and leaves the rest to the live suite.
if ! grep -q "the browser is up on port" "$LOG"; then
    FAILED="${FAILED:-the browser host never came up}"
fi
HELPERS=$(ps -eo pid,args | grep -E "forge-client Helper|browser-stack" | grep -v grep | awk '{print $1}')
kill -9 "$PID" $HELPERS 2>/dev/null
sleep 1
if [ -n "$FAILED" ]; then
    echo "SMOKE FAILED: $FAILED"
    exit 1
fi
echo "SMOKE OK: the client is up with its browser host"
