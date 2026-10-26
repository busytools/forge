#!/bin/sh
# Serialise the gate machine-wide: two `just check` runs on one machine boot
# CEF hosts into each other, because nextest only serialises the live suite
# within its own run (`.config/nextest.toml`). macOS has no flock(1), so the
# lock is a file whose content is the holder's pid. It lives beside forge's
# other machine-local state, never the repo or a synced dir.
#
# The pid is written to a candidate first and the lock is published with a
# hard link, so a visible lock always names its owner. `mkdir` then a separate
# pid write leaves an ownerless window, and `mv cand lock` onto an existing
# directory moves INSIDE it rather than failing, so neither is used.
#
# Usage:
#   check_lock.sh run [--label NAME] -- <command> [args...]
#
# Env:
#   FORGE_CHECK_LOCK_DIR      directory holding check.lock (tests)
#   FORGE_CHECK_LOCK_TIMEOUT  seconds to wait for a holder (default 3600)
#   FORGE_CHECK_LOCK_TEST_RACE  tests only: a command run after a dead holder
#                               is read and before the reclaim moves the lock
#   FORGE_CHECK_LOCK_TEST_AFTER_FORK  tests only: a command run after the gate
#                               is forked and before its pid is recorded

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

label="check"
ownerless_grace=4
held_lock=0
candidate=""
child=""
pending=""

usage() {
    echo "[ERROR] $label: usage: check_lock.sh run [--label NAME] -- <command> [args...]" >&2
    exit 2
}

# Every failure ends on a verdict line on stdout, like the rest of the gate.
fail() {
    printf '%s\n' "$2" >&2
    printf '%s\n' "$3"
    exit "$1"
}

# Remove the lock only while its content still names us: a reclaim can move
# a dead holder's lock aside, and a runner that lost its lock that way must
# not delete whoever holds the path now. The content check is also what makes
# a signal that lands between the publish and its bookkeeping safe: the lock
# is already ours, so it is released.
release() {
    holder="$1"
    if [ -n "$candidate" ]; then
        rm -f "$candidate"
    fi
    held=$(cat "$lock" 2>/dev/null || true)
    if [ "$held" = "$holder" ]; then
        rm -f "$lock"
    elif [ "$held_lock" -eq 1 ]; then
        printf '[WARN] %s: the gate lock no longer names pid %s (now %s); part of this run was unprotected\n' \
            "$label" "$holder" "${held:-empty}" >&2
    fi
}

reclaim_attempt=0

# Reclaim a lock whose holder is gone. The rename is what keeps two waiters
# from both taking it: only one gets the file. The content is re-read from
# the renamed copy, because a holder can take the path between the liveness
# check and the move; that lock is put back, never deleted.
reclaim() {
    dead="$1"
    reclaim_attempt=$((reclaim_attempt + 1))
    reaped="$lock.reaped.$$.$reclaim_attempt"
    rm -rf "$reaped"
    if ! mv "$lock" "$reaped" 2>/dev/null; then
        return 1
    fi
    seen=$(cat "$reaped" 2>/dev/null || true)
    if [ "$seen" != "$dead" ]; then
        # Publish the same content only if the path is still free; `ln` fails
        # on an existing file, and the content check catches a directory.
        if [ -n "$seen" ] && ln "$reaped" "$lock" 2>/dev/null &&
            [ "$(cat "$lock" 2>/dev/null || true)" = "$seen" ]; then
            rm -rf "$reaped"
            return 1
        fi
        rm -f "$lock/${reaped##*/}" 2>/dev/null || true
        printf '[ERROR] %s: a reclaim raced another holder (pid %s took the lock while pid %s was checked); the moved lock is at %s and %s is occupied\n' \
            "$label" "${seen:-unknown}" "${dead:-unknown}" "$reaped" "$lock" >&2
        printf '[ERROR] %s: the gate lock was not acquired (a reclaim raced pid %s)\n' \
            "$label" "${seen:-unknown}"
        exit 1
    fi
    if [ -n "$dead" ]; then
        printf '[..] %s: reclaimed a stale lock (pid %s is gone)\n' "$label" "$dead"
    else
        printf '[..] %s: reclaimed an ownerless lock\n' "$label"
    fi
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
            fail 2 \
                "[ERROR] $label: FORGE_CHECK_LOCK_TIMEOUT must be a whole number of seconds" \
                "[ERROR] $label: the gate lock was not acquired"
            ;;
    esac

    if ! mkdir -p "$lockroot" 2>/dev/null; then
        fail 2 \
            "[ERROR] $label: cannot create $lockroot" \
            "[ERROR] $label: the gate lock was not acquired"
    fi

    candidate="$lock.candidate.$owner"
    if ! printf '%s\n' "$owner" > "$candidate" 2>/dev/null; then
        fail 2 \
            "[ERROR] $label: cannot write $candidate" \
            "[ERROR] $label: the gate lock was not acquired"
    fi

    waited=0
    announced=0
    ownerless_since=-1
    while :; do
        if [ -n "$pending" ]; then
            exit "$pending"
        fi
        if ln "$candidate" "$lock" 2>/dev/null &&
            [ "$(cat "$lock" 2>/dev/null || true)" = "$owner" ]; then
            rm -f "$candidate"
            candidate=""
            held_lock=1
            return 0
        fi
        # `ln` treats an existing directory as a container; remove only the
        # link it made under that name, never the path itself.
        rm -f "$lock/${candidate##*/}" 2>/dev/null || true

        holder=$(cat "$lock" 2>/dev/null || true)
        if [ -z "$holder" ]; then
            # A visible lock always names its owner, so an ownerless one is a
            # leftover. Reclaim it after a grace, so a torn read cannot take
            # a live one.
            if [ "$ownerless_since" -lt 0 ]; then
                ownerless_since="$waited"
            fi
            if [ $((waited - ownerless_since)) -ge "$ownerless_grace" ]; then
                if reclaim ""; then
                    ownerless_since=-1
                    continue
                fi
            fi
        else
            ownerless_since=-1
            if ! kill -0 "$holder" 2>/dev/null; then
                if [ -n "${FORGE_CHECK_LOCK_TEST_RACE:-}" ]; then
                    sh -c "$FORGE_CHECK_LOCK_TEST_RACE" 2>/dev/null || true
                fi
                if reclaim "$holder"; then
                    continue
                fi
            fi
        fi

        if [ "$waited" -ge "$timeout" ]; then
            holder=$(cat "$lock" 2>/dev/null || true)
            printf '[ERROR] %s: gave up after %ss waiting for the gate lock held by pid %s; remove %s if that pid is not a gate run\n' \
                "$label" "$timeout" "${holder:-unknown}" "$lock" >&2
            printf '[ERROR] %s: the gate lock was not acquired (held by pid %s)\n' \
                "$label" "${holder:-unknown}"
            exit 1
        fi
        if [ "$announced" -eq 0 ]; then
            printf '[..] %s: waiting for another gate run (held by pid %s)\n' \
                "$label" "${holder:-unknown}"
            announced=1
        fi
        sleep 1
        waited=$((waited + 1))
    done
}

# TERM the gate and every process it started, then wait for the child before
# the EXIT trap frees the lock: a direct-child TERM leaves the group shells
# and their cargo/nextest/CEF children running beside the next gate. The tree
# is walked rather than a process group signalled because `set -m` is not
# portable: dash ignores it in a script, and background jobs there stay in
# the parent's group.
kill_tree() {
    ps -axo pid=,ppid= | awk -v root="$1" '
        { kids[$2] = kids[$2] " " $1 }
        END {
            frontier = root
            while (frontier != "") {
                n = split(frontier, f, " ")
                nxt = ""
                for (i = 1; i <= n; i++) {
                    m = split(kids[f[i]], c, " ")
                    for (j = 1; j <= m; j++) {
                        print c[j]
                        nxt = nxt " " c[j]
                    }
                }
                frontier = nxt
            }
        }' | while IFS= read -r pid; do
        kill -TERM "$pid" 2>/dev/null || true
    done
    kill -TERM "$1" 2>/dev/null || true
}

stop_child() {
    if [ -z "$child" ]; then
        # The signal beat the fork. Remember it: the child about to start is
        # stopped after it does, rather than left holding the pipes.
        pending="$1"
        return 0
    fi
    kill_tree "$child"
    wait "$child" 2>/dev/null || true
    exit "$1"
}

run() {
    held_lock=0
    candidate=""
    child=""
    pending=""
    trap 'release "$$"' EXIT
    trap 'stop_child 130' INT
    trap 'stop_child 143' TERM
    acquire "$$"
    "$@" &
    if [ -n "${FORGE_CHECK_LOCK_TEST_AFTER_FORK:-}" ]; then
        sh -c "$FORGE_CHECK_LOCK_TEST_AFTER_FORK" 2>/dev/null || true
    fi
    child=$!
    if [ -n "$pending" ]; then
        stop_child "$pending"
    fi
    status=0
    wait "$child" 2>/dev/null || status=$?
    exit "$status"
}

command="${1:-}"
if [ "$#" -gt 0 ]; then
    shift
fi
case "$command" in
    run)
        if [ "${1:-}" = "--label" ]; then
            if [ "$#" -lt 2 ]; then
                usage
            fi
            label="$2"
            shift 2
        fi
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
