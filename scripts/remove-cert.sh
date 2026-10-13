#!/usr/bin/env bash
# scripts/remove-cert.sh - remove forge's old CA from the macOS System
# keychain and delete its key material.
#
# forge used to run an HTTPS rewriting proxy and generated a per-machine
# CA for it, trusted as a root so tools inside a session (gh, curl, npm)
# would accept the intercepted traffic. Nothing in forge uses that CA any
# more, but a machine that ran the old installer still trusts it, and a
# trusted root nobody needs is worse than one in use.
#
# Idempotent: reports and exits 0 when there is nothing left to remove.
# Removing a trusted root needs an admin authorisation, so expect one
# sudo / Touch ID prompt when a cert is actually present.
#
# Env overrides:
#   FORGE_CA_DIR   CA directory (default: both app-support names are tried)

set -euo pipefail

# The CA was installed under the legacy `forge-tui` name, and the
# app-support tree later moved to `forge` - so the key material sits under
# whichever name was current when it was written: `forge/ca` on a machine
# that has run the migration, `forge-tui/ca` on one that has not. Both are
# tried unless FORGE_CA_DIR overrides.
CA_DIRS=()
if [ -n "${FORGE_CA_DIR:-}" ]; then
    CA_DIRS=("$FORGE_CA_DIR")
else
    CA_DIRS=(
        "$HOME/Library/Application Support/forge/ca"
        "$HOME/Library/Application Support/forge-tui/ca"
    )
fi
# `security find-certificate -c <name>` matches the CN field only, not
# the O. The old CA carried CN="forge wire-classification rewriter",
# O="forge-tui", so the lookup MUST use the CN - matching by O silently
# returns nothing and the cert reads as already gone.
FORGE_CA_CN="forge wire-classification rewriter"
KEYCHAIN="/Library/Keychains/System.keychain"

log_info()    { printf '\033[0;34m[INFO]\033[0m %s\n' "$*"; }
log_success() { printf '\033[0;32m[OK]\033[0m %s\n' "$*"; }
die()         { printf '\033[0;31m[ERROR]\033[0m %s\n' "$*" >&2; exit 1; }

is_ca_trusted() {
    security find-certificate -c "$FORGE_CA_CN" "$KEYCHAIN" >/dev/null 2>&1
}

if is_ca_trusted; then
    log_info "Removing forge CA from the System keychain (sudo / Touch ID prompt)"
    # Repeat until clean: a drifted machine can hold several copies and
    # delete-certificate removes one per call.
    while is_ca_trusted; do
        sudo security delete-certificate -c "$FORGE_CA_CN" "$KEYCHAIN" \
            || die "Failed to remove forge CA from $KEYCHAIN"
    done
    log_success "forge CA removed from the System keychain"
else
    log_info "forge CA is not trusted in the System keychain, nothing to remove"
fi

removed=0
for dir in "${CA_DIRS[@]}"; do
    if [ -d "$dir" ]; then
        rm -rf "$dir"
        log_success "Deleted CA key material at $dir"
        removed=1
    fi
done
if [ "$removed" -eq 0 ]; then
    log_info "No CA key material under the app-support CA names"
fi
