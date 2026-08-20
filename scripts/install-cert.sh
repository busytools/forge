#!/usr/bin/env bash
# scripts/install-cert.sh - install (or report on, or uninstall)
# forge's wire-classification rewriter CA cert as a trusted root in
# the macOS System keychain.
#
# Why this matters: forge's spawned `claude` subprocess gets the CA
# wired up via `NODE_EXTRA_CA_CERTS`, but other tools running inside
# a forge session (gh, curl, npm, …) consult the System keychain.
# Without trust there, those tools hit TLS cert errors when the
# rewriter intercepts their traffic.
#
# Content-idempotent: compares the SHA-256 fingerprint of the CA on
# disk against the cert in the keychain. If they match (one copy,
# same bytes), do nothing - no sudo, no Touch ID, no work. If they
# differ (missing, multiple copies, drifted), drop any existing
# copies and add the fresh one. Touch ID only fires on a genuine
# refresh, i.e. first install or after a CA rotation. macOS 26 locks
# trust-settings writes behind an admin authorisation, so the prompt is
# unavoidable when a refresh is genuinely needed; gating on the
# fingerprint compare is what keeps it rare.
#
# Usage:
#   scripts/install-cert.sh             # install / refresh (default)
#   scripts/install-cert.sh --status    # report-only, no changes
#   scripts/install-cert.sh --uninstall # remove the CA from keychain
#
# Env overrides:
#   FORGE_CA_PATH   MITM CA file path  (default: ~/Library/Application Support/forge-tui/ca/ca-cert.pem)

set -euo pipefail

MODE="install"
while [ $# -gt 0 ]; do
    case "$1" in
        --status)
            MODE="status"
            shift
            ;;
        --uninstall)
            MODE="uninstall"
            shift
            ;;
        -h|--help)
            sed -n '2,28p' "$0"
            exit 0
            ;;
        *)
            echo "[ERROR] Unknown arg: $1 (try --help)" >&2
            exit 2
            ;;
    esac
done

FORGE_CA_PATH="${FORGE_CA_PATH:-$HOME/Library/Application Support/forge-tui/ca/ca-cert.pem}"
# `security find-certificate -c <name>` matches the CN field only,
# not the O (organisation). forge's CA carries:
#   subject = CN="forge wire-classification rewriter", O="forge-tui"
# so the lookup MUST use the CN. Matching by O silently returns
# nothing - the script then thinks the cert is missing and triggers
# a fresh `security add-trusted-cert` (Touch ID prompt) on every run.
FORGE_CA_CN="forge wire-classification rewriter"
KEYCHAIN="/Library/Keychains/System.keychain"

log_info()    { printf '\033[0;34m[INFO]\033[0m %s\n' "$*"; }
log_warn()    { printf '\033[0;33m[WARN]\033[0m %s\n' "$*"; }
log_success() { printf '\033[0;32m[OK]\033[0m %s\n' "$*"; }
die()         { printf '\033[0;31m[ERROR]\033[0m %s\n' "$*" >&2; exit 1; }

is_ca_trusted() {
    security find-certificate -c "$FORGE_CA_CN" "$KEYCHAIN" >/dev/null 2>&1
}

case "$MODE" in
    status)
        if is_ca_trusted; then
            log_success "forge CA is trusted in the System keychain"
            exit 0
        else
            log_warn "forge CA is NOT trusted in the System keychain"
            if ! [ -f "$FORGE_CA_PATH" ]; then
                log_info "  (CA file also missing at $FORGE_CA_PATH; launch forge once to generate)"
            fi
            exit 1
        fi
        ;;
    uninstall)
        if ! is_ca_trusted; then
            log_info "forge CA is not currently trusted, nothing to remove"
            exit 0
        fi
        log_info "Removing forge CA from System keychain (Touch ID / sudo prompt)"
        if sudo security delete-certificate -c "$FORGE_CA_CN" "$KEYCHAIN"; then
            log_success "forge CA removed from System keychain"
        else
            die "Failed to remove forge CA"
        fi
        exit 0
        ;;
esac

# install mode below.
if ! [ -f "$FORGE_CA_PATH" ]; then
    log_info "forge MITM CA not generated yet at:"
    log_info "  $FORGE_CA_PATH"
    log_info "Launch forge once to generate it, then re-run scripts/install-cert.sh"
    log_info "to add it to the System keychain. Without trust, gh / curl"
    log_info "inside a forge session will hit cert errors."
    exit 0
fi

# Disk CA fingerprint (SHA-256, normalised: uppercase, no colons).
disk_fp=$(openssl x509 -in "$FORGE_CA_PATH" -noout -sha256 -fingerprint 2>/dev/null \
    | sed 's/.*=//' | tr -d ':' | tr '[:lower:]' '[:upper:]')
if [[ -z "$disk_fp" ]]; then
    log_warn "Could not read forge CA fingerprint from $FORGE_CA_PATH; skipping trust step"
    exit 0
fi

# All forge-tui certs in keychain, their SHA-256s.
keychain_fps=$(/usr/bin/security find-certificate -c "$FORGE_CA_CN" -a -Z "$KEYCHAIN" 2>/dev/null \
    | awk '/^SHA-256 hash:/ {print $NF}')
kfp_count=$(printf '%s' "$keychain_fps" | grep -c . || true)

# Common path on repeat installs: exactly one keychain copy matching
# disk. No-op - no sudo, no Touch ID.
if [[ "$kfp_count" -eq 1 ]] && printf '%s' "$keychain_fps" | grep -qFx "$disk_fp"; then
    log_info "forge CA in keychain matches disk - no refresh needed"
    exit 0
fi

# Refresh path: Touch ID will fire once for the add step.
if [[ "$kfp_count" -eq 0 ]]; then
    log_info "forge CA not in keychain - installing (Touch ID prompt)"
elif [[ "$kfp_count" -gt 1 ]]; then
    log_info "$kfp_count forge CA copies in keychain - cleaning up + reinstalling (Touch ID prompt)"
else
    log_info "forge CA in keychain is stale - refreshing (Touch ID prompt)"
fi

# Drop any existing forge-tui certs (handles drift + multi-copy).
while is_ca_trusted; do
    if ! sudo /usr/bin/security delete-certificate -c "$FORGE_CA_CN" "$KEYCHAIN" >/dev/null 2>&1; then
        log_warn "Failed to remove a stale forge CA copy; continuing"
        break
    fi
done

if sudo /usr/bin/security add-trusted-cert -d -r trustRoot -k "$KEYCHAIN" "$FORGE_CA_PATH"; then
    log_success "forge CA trusted in System keychain"
else
    log_warn "CA trust step failed or was cancelled - re-run scripts/install-cert.sh to retry"
    exit 0
fi
