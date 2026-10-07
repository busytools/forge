#!/usr/bin/env bash
# macOS copies a locally-signed bundle into a `X/<bundle-id>.code_sign_clone`
# directory on EVERY launch and nothing reaps them - the E2E loop accumulated
# 55G in four hours (the lead's disk finding, 2026-10-07).
#
# Deletes THIS bundle's dead clones only: an entry any live process holds
# open is left alone (the running app's clone is the live one), the match is
# the bundle id under the per-user X temp dir, and no other id is touched.
#
# Usage: reap-clones.sh <bundle identifier>
set -uo pipefail
ID="${1:?the bundle identifier}"
REAPED=0
for dir in /private/var/folders/*/*/X/"$ID"*.code_sign_clone*; do
    [ -d "$dir" ] || continue
    # `-t` prints PIDs only, no header: a header would match every grep and
    # read every clone as live (which is how a reaper reaps nothing).
    if [ -n "$(lsof -t +D "$dir" 2>/dev/null)" ]; then
        echo "live, kept: $dir"
        continue
    fi
    if rm -rf "$dir" 2>/dev/null; then
        REAPED=$((REAPED + 1))
        echo "reaped: $dir"
    fi
done
echo "clones reaped: $REAPED"
