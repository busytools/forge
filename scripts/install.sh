#!/usr/bin/env bash
# scripts/install.sh - build forge-tui from the current checkout and
# install the `forge` binary into cargo's default install root
# (~/.cargo/bin/forge).
#
# Sourced into `just install` via the workspace justfile. Standalone
# invocation also works for users who don't have `just` on PATH.
#
# Build source is THIS repo's working tree (the script's parent
# directory). Whatever branch is checked out is what gets built - the
# script does not clone, fetch, or reset.
#
# `--features perf` is enabled by default. forge's perf sidecar is
# zero-cost without `--perf-log <path>` (Timer drops short-circuit on
# the LOG_FILE check), so it stays compiled in unless you pass
# --no-perf.
#
# After install, the script regenerates the zsh completion at
# ~/.zsh/completions/_forge (override via FORGE_ZSH_COMPLETION_DIR)
# and best-effort runs scripts/install-cert.sh to refresh the
# wire-rewriter CA in the System keychain.
#
# Usage:
#   scripts/install.sh             # regular install (perf feature on)
#   scripts/install.sh --no-perf   # opt out of the perf sidecar
#
# Env overrides:
#   FORGE_ZSH_COMPLETION_DIR    zsh completion dir   (default: ~/.zsh/completions)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FORGE_REPO="$(cd "$SCRIPT_DIR/.." && pwd)"

PERF_FEATURE=1
while [ $# -gt 0 ]; do
    case "$1" in
        --no-perf)
            PERF_FEATURE=0
            shift
            ;;
        -h|--help)
            sed -n '2,30p' "$0"
            exit 0
            ;;
        *)
            echo "[ERROR] Unknown arg: $1 (try --help)" >&2
            exit 2
            ;;
    esac
done

FORGE_ZSH_COMPLETION_DIR="${FORGE_ZSH_COMPLETION_DIR:-$HOME/.zsh/completions}"

log_info()    { printf '\033[0;34m[INFO]\033[0m %s\n' "$*"; }
log_warn()    { printf '\033[0;33m[WARN]\033[0m %s\n' "$*"; }
log_success() { printf '\033[0;32m[OK]\033[0m %s\n' "$*"; }
die()         { printf '\033[0;31m[ERROR]\033[0m %s\n' "$*" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

if [ "$PERF_FEATURE" -eq 1 ]; then
    BUILD_LABEL="release+perf"
else
    BUILD_LABEL="release"
fi

require_cmd cargo
require_cmd git

[ -d "$FORGE_REPO/.git" ] \
    || die "forge checkout missing .git at $FORGE_REPO - script must run from inside the forge repo"
[ -d "$FORGE_REPO/crates/forge-tui" ] \
    || die "forge-tui crate not found at $FORGE_REPO/crates/forge-tui"

CURRENT_BRANCH="$(git -C "$FORGE_REPO" rev-parse --abbrev-ref HEAD)"
CURRENT_SHA="$(git -C "$FORGE_REPO" rev-parse --short HEAD)"

# Sweep the legacy ~/.cargo-forge install root if present. An older
# revision of this script installed forge there via cargo's --root
# flag; leaving the tree behind creates PATH ambiguity (~/.cargo/bin
# usually wins but a stale copy can shadow depending on PATH order).
# One canonical location only.
LEGACY_FORGE_ROOT="$HOME/.cargo-forge"
if [ -d "$LEGACY_FORGE_ROOT" ]; then
    log_info "Removing legacy install root at $LEGACY_FORGE_ROOT"
    # cargo also drops `.crates.toml` / `.crates2.json` tracking
    # files at the root; nuke the whole tree so nothing's left to
    # trip over.
    rm -rf "$LEGACY_FORGE_ROOT"
fi

log_info "Building forge-tui ($BUILD_LABEL - this can take a few minutes)"
CARGO_ARGS=(
    --path "$FORGE_REPO/crates/forge-tui"
    --bin forge
    --locked
    --force
)
if [ "$PERF_FEATURE" -eq 1 ]; then
    CARGO_ARGS+=(--features perf)
fi
# cd into FORGE_REPO so rustup picks up its rust-toolchain.toml - without
# this, cargo uses the global default toolchain which may be older than
# what forge-tui requires.
(
    cd "$FORGE_REPO"
    cargo install "${CARGO_ARGS[@]}"
)

# Resolve cargo's install root so the success line + completion step
# both reference the same path. `CARGO_INSTALL_ROOT` overrides the
# default, falling back to `$CARGO_HOME/bin` (`~/.cargo/bin`).
FORGE_BIN_DIR="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
FORGE_BIN="$FORGE_BIN_DIR/forge"

log_success "forge installed at $FORGE_BIN"
log_success "  forge @ $CURRENT_SHA (branch: $CURRENT_BRANCH, build: $BUILD_LABEL)"

if [ "$PERF_FEATURE" -eq 1 ]; then
    log_info "Capture frame timings with:"
    log_info "  $FORGE_BIN --perf-log /tmp/forge-perf.log"
fi

# zsh completion. The hidden `--generate-completion` flag landed with
# the workspace migration; older builds won't have it, so we silently
# skip when the binary doesn't recognise it.
COMPLETION_FILE="$FORGE_ZSH_COMPLETION_DIR/_forge"
if "$FORGE_BIN" --generate-completion zsh > /dev/null 2>&1; then
    mkdir -p "$FORGE_ZSH_COMPLETION_DIR"
    "$FORGE_BIN" --generate-completion zsh > "$COMPLETION_FILE"
    log_success "zsh completion installed at $COMPLETION_FILE"
    # Bust the zsh completion cache so the next interactive shell picks
    # up _forge. compinit keys its cached function table by file mtimes;
    # without this, the new _forge sits on fpath but the cache keeps
    # resolving from the old dump.
    rm -f "$HOME"/.zcompdump* 2>/dev/null || true
    if ! grep -q "$FORGE_ZSH_COMPLETION_DIR" "$HOME/.zshrc" 2>/dev/null; then
        log_info "Add this to your ~/.zshrc to enable completion:"
        log_info "  fpath=($FORGE_ZSH_COMPLETION_DIR \$fpath)"
        log_info "  autoload -U compinit && compinit"
    fi
else
    log_warn "forge --generate-completion not supported by this build; skipping zsh completion"
fi

# Refresh the wire-rewriter CA in the System keychain. Best-effort:
# install-cert.sh's content-idempotent compare returns 0 even when no
# work is needed; an actual refresh might trigger a Touch ID prompt
# but we still don't want this to fail the binary install.
if [ -x "$SCRIPT_DIR/install-cert.sh" ]; then
    "$SCRIPT_DIR/install-cert.sh" || log_warn "install-cert.sh returned non-zero; run again manually if needed"
fi
