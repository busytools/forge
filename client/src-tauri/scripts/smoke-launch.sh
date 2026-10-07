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
# The checks run while the app is LIVE: its CDP port dies with it.
FAILED=""
if [ -z "$ALIVE" ]; then
    FAILED="the client died on launch"
fi
if grep -q "CEF stays off\|CEF did not initialize\|browser stays off\|NSApplication was created before" "$LOG"; then
    FAILED="CEF did not come up"
fi
if ! grep -q "the browser view is up" "$LOG"; then
    FAILED="${FAILED:-the browser view was not created}"
fi
# The engine's own announcement, then its endpoint actually answering: a
# CEF that initialized but cannot be reached is not up.
CEF_LINE=$(grep -o "CEF is up, debugging on 127.0.0.1:[0-9]*" "$LOG" | head -1)
PORT=""
if [ -z "$CEF_LINE" ]; then
    FAILED="${FAILED:-CEF never came up (no debug-port line)}"
else
    PORT="${CEF_LINE##*:}"
    if ! curl -s --max-time 3 "http://127.0.0.1:$PORT/json/version" | grep -q "webSocketDebuggerUrl"; then
        FAILED="CEF's CDP on port $PORT does not answer"
    fi
fi
HELPERS=$(ps -eo pid,args | grep "forge-client Helper" | grep -v grep | awk '{print $1}')
kill -9 "$PID" $HELPERS 2>/dev/null
sleep 1
if [ -n "$FAILED" ]; then
    echo "SMOKE FAILED: $FAILED"
    # **One browser per profile, one client per machine.** A second client's
    # CEF listens but never serves while another instance holds the profile,
    # so name the cause rather than letting it read as a broken build.
    OTHERS=$(ps -eo pid,args | grep "MacOS/forge-client" | grep -v grep | grep -v " $PID " | wc -l | tr -d ' ')
    if [ "$OTHERS" != "0" ]; then
        echo "NOTE: $OTHERS other forge-client instance(s) are running - two clients share one browser profile"
    fi
    exit 1
fi
echo "SMOKE OK: the client is up with CEF on 127.0.0.1:$PORT"
