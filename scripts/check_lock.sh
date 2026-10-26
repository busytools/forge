#!/bin/sh
# Serialise the gate machine-wide: two `just check` runs on one machine boot
# CEF hosts into each other, because nextest only serialises the live suite
# within its own run (`.config/nextest.toml`). macOS has no flock(1), so the
# lock is an atomically created directory holding the holder's pid. It lives
# beside forge's other machine-local state, never the repo or a synced dir.
#
# Usage:
#   check_lock.sh run -- <command> [args...]   acquire, run, release
#
# Env:
#   FORGE_CHECK_LOCK_DIR      directory holding check.lock (tests)
#   FORGE_CHECK_LOCK_TIMEOUT  seconds to wait for a holder (default 3600)

set -u

if [ -n "${FORGE_CHECK_LOCK_DIR:-}" ]; then
    lockroot="$FORGE_CHECK_LOCK_DIR"
else
    if [ -z "${HOME:-}" ]; then
        echo "[ERROR] check: no HOME to place the gate lock under" >&2
        exit 2
    fi
    case "$(uname -s)" in
        Darwin) lockroot="$HOME/Library/Application Support/forge" ;;
        *) lockroot="${XDG_DATA_HOME:-$HOME/.local/share}/forge" ;;
    esac
fi
lock="$lockroot/check.lock"

usage() {
    echo "[ERROR] check: usage: check_lock.sh run -- <command> [args...]" >&2
    exit 2
}

# Remove the lock only while its pid still names us: a reclaim renames a
# dead holder's directory aside, and a runner that lost its lock that way
# must not delete whoever holds the path now.
release() {
    held=$(cat "$lock/pid" 2>/dev/null || true)
    if [ "$held" = "$1" ]; then
        rm -rf "$lock"
    fi
}

reclaim_attempt=0

# Reclaim a lock whose holder is gone. The rename is what keeps two waiters
# from both taking it: only one gets the directory. The pid is re-read from
# the renamed copy, because a racing holder can take the lock between the
# liveness check and the rename; that lock is put back, never deleted.
reclaim() {
    reclaim_attempt=$((reclaim_attempt + 1))
    reaped="$lock.reaped.$$.$reclaim_attempt"
    rm -rf "$reaped"
    if ! mv "$lock" "$reaped" 2>/dev/null; then
        return 1
    fi
    seen=$(cat "$reaped/pid" 2>/dev/null || true)
    if [ "$seen" != "$1" ]; then
        # mkdir claims the path first: a bare move would land INSIDE a
        # directory another acquirer had just made.
        if mkdir "$lock" 2>/dev/null; then
            if mv "$reaped/pid" "$lock/pid" 2>/dev/null; then
                rm -rf "$reaped"
                return 1
            fi
            rm -rf "$lock"
        fi
        echo "[ERROR] check: a reclaim raced a live holder (pid ${seen:-unknown} took the lock while pid $1 was checked); $lock is occupied and the live lock is at $reaped - nothing was deleted" >&2
        exit 1
    fi
    echo "[..] check: reclaimed a stale lock (pid $1 is gone)"
    rm -rf "$reaped"
    return 0
}

acquire() {
    owner="$1"
    case "$owner" in
        ''|*[!0-9]*) usage ;;
    esac
    timeout="${FORGE_CHECK_LOCK_TIMEOUT:-3600}"
    case "$timeout" in
        ''|*[!0-9]*)
            echo "[ERROR] check: FORGE_CHECK_LOCK_TIMEOUT must be a whole number of seconds" >&2
            exit 2
            ;;
    esac

    mkdir -p "$lockroot" 2>/dev/null || {
        echo "[ERROR] check: cannot create $lockroot" >&2
        exit 2
    }

    waited=0
    announced=0
    while :; do
        if mkdir "$lock" 2>/dev/null; then
            if ! printf '%s\n' "$owner" > "$lock/pid" 2>/dev/null; then
                rm -rf "$lock"
                echo "[ERROR] check: cannot write $lock/pid" >&2
                exit 2
            fi
            return 0
        fi

        holder=$(cat "$lock/pid" 2>/dev/null || true)
        if [ -n "$holder" ] && ! kill -0 "$holder" 2>/dev/null; then
            if reclaim "$holder"; then
                continue
            fi
        fi

        if [ "$waited" -ge "$timeout" ]; then
            echo "[ERROR] check: gave up after ${timeout}s waiting for the gate lock held by pid ${holder:-unknown}" >&2
            echo "        if that pid is not a gate run, remove $lock" >&2
            exit 1
        fi
        if [ "$announced" -eq 0 ]; then
            echo "[..] check: waiting for another gate run (held by pid ${holder:-unknown})"
            announced=1
        fi
        sleep 2
        waited=$((waited + 2))
    done
}

child=""

stop_child() {
    if [ -n "$child" ]; then
        kill -TERM "$child" 2>/dev/null || true
    fi
    exit "$1"
}

run() {
    child=""
    trap 'release "$$"' EXIT
    trap 'stop_child 130' INT
    trap 'stop_child 143' TERM
    acquire "$$"
    status=0
    "$@" &
    child=$!
    wait "$child" || status=$?
    exit "$status"
}

command="${1:-}"
if [ "$#" -gt 0 ]; then
    shift
fi
case "$command" in
    run)
        if [ "${1:-}" = "--" ]; then
            shift
        fi
        if [ "$#" -eq 0 ]; then
            usage
        fi
        run "$@"
        ;;
    *) usage ;;
esac
