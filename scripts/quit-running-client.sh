#!/usr/bin/env bash
# Quit a client running from the bundle before the bundle is replaced: a
# graceful SIGTERM, then a bounded wait for it to go. A client that survives
# is a refusal, because swapping the bundle underneath a live process is the
# one way this fails quietly.
#
# **The query names the process EXECUTING the client, not every process with
# its file open**: `-d txt` is the executable mapping, so an XPC or helper
# service that merely holds the binary open is neither signalled nor waited
# on. Without it, a file-holder that ignores TERM would refuse a release that
# should go ahead.
#
# The check is on the bundle's own executable, so a client running from a
# checkout's target dir does not block a release - it goes on executing the
# image it started from, which this neither disturbs nor updates.
#
# Usage: quit-running-client.sh <app> [bound-seconds]
set -euo pipefail

app=${1:?the app bundle path}
bound=${2:-15}
exe="$app/Contents/MacOS/forge-client"

# Fails closed: without lsof the check cannot answer, and proceeding is the
# failure it exists to prevent.
command -v lsof >/dev/null 2>&1 || {
    echo "[ERROR] lsof not found - cannot tell whether the client is running" >&2
    exit 1
}
# No bundle, nothing in use - and lsof prints its banner and usage for a path
# it cannot find.
[ -e "$exe" ] || exit 0

pids=$(lsof -a -t -d txt "$exe" || true)
[ -n "$pids" ] || exit 0

echo "[INFO] quitting the client at $app (pid $(echo "$pids" | tr '\n' ' '))" >&2
# Word splitting is the point: lsof prints one pid per line.
# shellcheck disable=SC2086
kill $pids 2>/dev/null || true

deadline=$((SECONDS + bound))
while :; do
    pids=$(lsof -a -t -d txt "$exe" || true)
    [ -n "$pids" ] || exit 0
    if [ "$SECONDS" -ge "$deadline" ]; then
        echo "[ERROR] $app is in use (pid $(echo "$pids" | tr '\n' ' ')) - the client did not quit within ${bound}s" >&2
        exit 1
    fi
    sleep 0.2
done
