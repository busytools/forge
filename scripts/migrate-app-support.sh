#!/usr/bin/env bash
# scripts/migrate-app-support.sh - move forge's app-support tree from its
# legacy `forge-tui` name to `forge`, once.
#
# The directory holds live state - the single-instance lock, the redb
# store (crons, Gotify/Slack subscriptions, session ids, usage cache) and
# the logs - so the new `forge` binary REFUSES to boot while only the
# legacy tree exists. Run this once, then boot.
#
# Safe to re-run: with nothing to move it says so and exits 0. A legacy
# tree left beside a live current one is reported and left alone.
#
# Env overrides:
#   FORGE_APP_SUPPORT_BASE   the directory the two names live under
#                             (default: the platform app-support base)

set -euo pipefail

log_info()    { printf '\033[0;34m[INFO]\033[0m %s\n' "$*"; }
log_success() { printf '\033[0;32m[OK]\033[0m %s\n' "$*"; }
log_warn()    { printf '\033[0;33m[WARN]\033[0m %s\n' "$*"; }

# The same candidate order `forge_sdk::paths::app_support_dir` resolves:
# the platform data dir, then the cache dir, then `$HOME`.
pick_base() {
    if [ -n "${FORGE_APP_SUPPORT_BASE:-}" ]; then
        printf '%s\n' "$FORGE_APP_SUPPORT_BASE"
        return
    fi
    case "$(uname -s)" in
        Darwin) printf '%s\n' "$HOME/Library/Application Support" ;;
        *) printf '%s\n' "${XDG_DATA_HOME:-$HOME/.local/share}" ;;
    esac
}

BASE="$(pick_base)"
LEGACY="$BASE/forge-tui"
CURRENT="$BASE/forge"

report() {
    local dir="$1"
    local label="$2"
    local count
    count=$(find "$dir" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')
    log_info "$label: $dir ($count entries)"
    find "$dir" -mindepth 1 -maxdepth 1 -exec basename {} \; | sed 's/^/    /'
}

if [ ! -d "$LEGACY" ]; then
    if [ -d "$CURRENT" ]; then
        log_success "nothing to migrate: $CURRENT is already the tree in use"
    else
        log_success "nothing to migrate: no app-support tree exists yet, and forge will create $CURRENT on its first boot"
    fi
    exit 0
fi

if [ -d "$CURRENT" ]; then
    log_warn "both trees exist: keeping $CURRENT as the one in use"
    log_info "the legacy tree is left for you to inspect and delete:"
    report "$LEGACY" "legacy"
    exit 0
fi

log_info "moving the app-support tree:"
report "$LEGACY" "from"
mv "$LEGACY" "$CURRENT"
log_success "moved to $CURRENT"
log_info "the lock is per-config-dir hashed, so it migrated with the directory; sessions, crons and subscriptions are intact"
