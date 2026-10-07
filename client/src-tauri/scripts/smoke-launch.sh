#!/usr/bin/env bash
# Launch a built forge.app briefly and refuse a silent failure.
#
# **A client that dies on launch is a shipped-broken artifact**, and neither
# the compiler nor the bundler can see it. This runs the app's own binary,
# watches it live in a window of seconds, reads its words, and exits
# non-zero when it crashed or when its browser host never came up. The e2e
# and release flows call THIS script; they do not re-implement it.
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
# **The host's own line is the engine's proof**: the browser came up, and
# the smoke checks the host's announcement and leaves the rest to the live
# suite.
if ! grep -q "the browser is up on port" "$LOG"; then
    FAILED="${FAILED:-the browser host never came up}"
fi
# The browser dies by PORT, never by a process-name pattern: a pattern would
# reach a browser another session launched. The host names its port in the
# very line the check above reads.
PORT=$(sed -n 's/.*the browser is up on port \([0-9][0-9]*\).*/\1/p' "$LOG" | head -1)
kill -9 "$PID" 2>/dev/null
if [ -n "$PORT" ]; then
    BROWSE_PIDS=$(lsof -t -iTCP:"$PORT" -sTCP:LISTEN 2>/dev/null || true)
    if [ -n "$BROWSE_PIDS" ]; then
        kill -9 $BROWSE_PIDS 2>/dev/null
    fi
fi
sleep 1
if [ -n "$FAILED" ]; then
    echo "SMOKE FAILED: $FAILED"
    exit 1
fi
echo "SMOKE OK: the client is up with its browser host"
