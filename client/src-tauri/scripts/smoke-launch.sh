#!/usr/bin/env bash
# Launch a built forge.app briefly and refuse a silent failure.
#
# **A client that dies on launch is a shipped-broken artifact**, and neither
# the compiler nor the bundler can see it. This runs the app's own binary,
# watches it live, reads its words, and exits non-zero when it crashed or
# when its browser host never came up. The e2e flow calls THIS script; run it
# by hand against a release bundle before shipping one.
#
# **The wait is on the browser's own line, bounded by the launch timeout**:
# the app boots in about a second but the browser behind it is given fifteen,
# so a fixed six-second window failed a slow-but-fine start.
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
for _ in $(seq 1 20); do
    grep -q "the browser is up on port" "$LOG" && break
    sleep 1
done
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
#
# **And only when this run LAUNCHED it.** A run that merely ATTACHED to a
# browser already up (one a previous run left detached) must not kill it:
# measured 2026-10-07, a smoke run killed a reused shared browser. A browser
# younger than this run's own elapsed time is the one this run started.
PORT=$(sed -n 's/.*the browser is up on port \([0-9][0-9]*\).*/\1/p' "$LOG" | head -1)
kill -9 "$PID" 2>/dev/null
etime_seconds() {
    awk -F'[-:]' -v when="$1" '{
        if (NF == 2) print $1 * 60 + $2;
        else if (NF == 3) print $1 * 3600 + $2 * 60 + $3;
        else print $1 * 86400 + $2 * 3600 + $3 * 60 + $4;
    }' <<< "$1"
}
RUN_SECS=$(etime_seconds "$(ps -p $$ -o etime= | tr -d ' ')")
if [ -n "$PORT" ]; then
    for browse in $(lsof -t -iTCP:"$PORT" -sTCP:LISTEN 2>/dev/null || true); do
        BROWSE_SECS=$(etime_seconds "$(ps -p "$browse" -o etime= | tr -d ' ')")
        if [ -n "$BROWSE_SECS" ] && [ "$BROWSE_SECS" -le "$((RUN_SECS + 2))" ]; then
            kill -9 "$browse" 2>/dev/null
        else
            echo "left alone: the browser on $PORT (pid $browse) was up before this run (browser ${BROWSE_SECS:-?}s, run ${RUN_SECS:-?}s)"
        fi
    done
fi
sleep 1
if [ -n "$FAILED" ]; then
    echo "SMOKE FAILED: $FAILED"
    exit 1
fi
echo "SMOKE OK: the client is up with its browser host"
