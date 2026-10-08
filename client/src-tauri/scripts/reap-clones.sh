#!/usr/bin/env bash
# macOS copies a locally-signed bundle into a `X/<bundle-id>.code_sign_clone`
# directory on EVERY launch and nothing reaps them - the E2E loop accumulated
# clones at a rate the lead caught as a disk finding (2026-10-07).
#
# **The cost, re-measured the same day (the lead's ask): two dead clones
# whose `du` read 7.2G freed 18 MB of df free space.** The du number is hard
# links and CoW clones, not disk pressure; reap for the litter and the
# inodes, and never quote du as pressure for these.
#
# Deletes dead clones of the ids it is given, and only those: a clone is kept
# when a LIVE PROCESS runs from inside it.
#
# **Liveness is a process whose own executable path sits under the clone, not
# `lsof +D`.** A clone is a hard-link farm: `lsof +D` matches by (device,
# inode), so a live process holding a file in ITS clone makes that inode read
# as open in every other clone sharing it - every clone looked live while any
# instance ran, which is exactly when the reaper matters (the lead's read,
# 2026-10-07).
#
# Usage: reap-clones.sh <bundle identifier> [more identifiers ...]
#        reap-clones.sh --self-test   prove the liveness matcher both ways
set -uo pipefail

live_run_from() {
    # Pids whose argv names an executable under this directory.
    local dir="$1" pid args
    while read -r pid args; do
        case "$args" in
            "$dir"/*) printf '%s ' "$pid" ;;
        esac
    done < <(ps -eo pid=,args=)
}

# The matcher carries the whole decision, and a wrong matcher reaps a running
# app's clone, so it proves itself here rather than in the field.
if [ "${1:-}" = "--self-test" ]; then
    D="/tmp/example.code_sign_clone"
    match() { case "$1" in "$D"/*) echo yes ;; *) echo no ;; esac; }
    [ "$(match "$D/fake-app 300")" = yes ] || { echo "self-test FAILED: a process from the dir must match"; exit 1; }
    [ "$(match "/bin/sleep 300 $D/x")" = no ] || { echo "self-test FAILED: the dir inside another command's args must not match"; exit 1; }
    [ "$(match "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser")" = no ] || { echo "self-test FAILED: a plain path must not match"; exit 1; }
    echo "self-test OK"
    exit 0
fi

REAPED=0
KEPT=0
for ID in "$@"; do
    for dir in /private/var/folders/*/*/X/"$ID"*.code_sign_clone*; do
        [ -d "$dir" ] || continue
        live="$(live_run_from "$dir")"
        if [ -n "$live" ]; then
            echo "live, kept: $dir (pids: $live)"
            KEPT=$((KEPT + 1))
            continue
        fi
        if rm -rf "$dir" 2>/dev/null; then
            REAPED=$((REAPED + 1))
            echo "reaped: $dir"
        fi
    done
done
echo "clones reaped: $REAPED, kept live: $KEPT"
