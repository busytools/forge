#!/usr/bin/env bash
# scripts/install.sh - build forge from the current checkout and
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
# After install, the script regenerates the zsh completion at
# ~/.zsh/completions/_forge (override via FORGE_ZSH_COMPLETION_DIR).
#
# Usage:
#   scripts/install.sh
#
# Env overrides:
#   FORGE_ZSH_COMPLETION_DIR    zsh completion dir   (default: ~/.zsh/completions)
#   FORGE_INSTALL_BROWSER       0 skips the macOS browser stage (default: 1)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FORGE_REPO="$(cd "$SCRIPT_DIR/.." && pwd)"

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help)
            sed -n '2,22p' "$0"
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

BUILD_LABEL="release"

require_cmd cargo
require_cmd git

# `-e` not `-d`: in a git worktree `.git` is a file holding a gitdir:
# pointer, and every worker session runs in one.
[ -e "$FORGE_REPO/.git" ] \
    || die "forge checkout missing .git at $FORGE_REPO - script must run from inside the forge repo"
[ -d "$FORGE_REPO/crates/forge-server" ] \
    || die "forge-server crate not found at $FORGE_REPO/crates/forge-server"

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

log_info "Building forge ($BUILD_LABEL - this can take a few minutes)"
# Install into a staging root first: the live binary is replaced only after
# the new one has started with its libraries beside it.
STAGING_ROOT="$FORGE_REPO/target/install-root"
rm -rf "$STAGING_ROOT"
mkdir -p "$STAGING_ROOT"
CARGO_ARGS=(
    --path "$FORGE_REPO/crates/forge-server"
    --bin forge
    --locked
    --force
    --root "$STAGING_ROOT"
    # The build must land in the repo's own target dir: the transcribe
    # libraries forge-dictate stages beside the profile are what gets
    # installed next to the binary, and `cargo install` otherwise builds in a
    # temporary dir it deletes.
    --target-dir "$FORGE_REPO/target"
)
# cd into FORGE_REPO so rustup picks up its rust-toolchain.toml - without
# this, cargo uses the global default toolchain which may be older than
# what forge requires.
(
    cd "$FORGE_REPO"
    # Marks the binary as built through this script. A hand-rolled
    # `cargo install` leaves it unset, and the binary then warns at
    # startup that its dependency graph may not be the locked one -
    # `cargo install` ignores Cargo.lock unless `--locked` is passed.
    FORGE_BUILD_PROVENANCE=install.sh cargo install "${CARGO_ARGS[@]}"
)

# Resolve cargo's install root so the success line + completion step
# both reference the same path. `CARGO_INSTALL_ROOT` overrides the
# default, falling back to `$CARGO_HOME/bin` (`~/.cargo/bin`).
FORGE_BIN_DIR="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
FORGE_BIN="$FORGE_BIN_DIR/forge"
STAGED_BIN="$STAGING_ROOT/bin/forge"

# transcribe's ggml is built shared, and the binary's rpath names its own
# directory, so the libraries ship beside it. The build staged them next to
# the release profile; without them the binary does not start. Cargo puts a
# target-triple build under target/<triple>/, and CARGO_BUILD_TARGET is how
# the triple is selected.
FORGE_TARGET_DIR="$FORGE_REPO/target${CARGO_BUILD_TARGET:+/$CARGO_BUILD_TARGET}"
FORGE_TRANSCRIBE_LIBS="$FORGE_TARGET_DIR/$BUILD_LABEL/transcribe-libs"
[ -d "$FORGE_TRANSCRIBE_LIBS" ] \
    || die "transcribe's shared libraries are not at $FORGE_TRANSCRIBE_LIBS: a target-triple build stages under target/<triple>/$BUILD_LABEL (CARGO_BUILD_TARGET selects it), and a static transcribe or a failed staging step leaves nothing there"
cp -R "$FORGE_TRANSCRIBE_LIBS/." "$STAGING_ROOT/bin/"

# The installed binary gets no library path, so neither does the check: the
# caller's environment must not be able to mask a missing library.
run_binary() {
    env -u LD_LIBRARY_PATH -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH "$@"
}
run_binary "$STAGED_BIN" --version > /dev/null \
    || die "the staged binary does not start - run $STAGED_BIN directly for the loader's error"

# The live install is replaced only now. Real files get a fresh inode through
# a temp name and a rename, so a forge already running keeps its mapped copy;
# the SONAME and link names are repointed after them; the binary follows by
# the same rename; the stale sweep runs last.
mkdir -p "$FORGE_BIN_DIR"
for staged in "$FORGE_TRANSCRIBE_LIBS"/*; do
    [ -e "$staged" ] || continue
    if [ -L "$staged" ]; then
        continue
    fi
    name="${staged##*/}"
    cp "$staged" "$FORGE_BIN_DIR/.$name.new"
    mv -f "$FORGE_BIN_DIR/.$name.new" "$FORGE_BIN_DIR/$name"
done
for staged in "$FORGE_TRANSCRIBE_LIBS"/*; do
    [ -L "$staged" ] || continue
    ln -sfn "$(readlink "$staged")" "$FORGE_BIN_DIR/${staged##*/}"
done
# Drop a library this build no longer ships, dangling links included; a stale
# name beside the binary could satisfy the loader with the wrong file.
for installed in "$FORGE_BIN_DIR"/libtranscribe* "$FORGE_BIN_DIR"/libggml*; do
    [ -e "$installed" ] || [ -L "$installed" ] || continue
    staged="$FORGE_TRANSCRIBE_LIBS/${installed##*/}"
    [ -e "$staged" ] || [ -L "$staged" ] || rm -f "$installed"
done
cp "$STAGED_BIN" "$FORGE_BIN_DIR/.forge.new"
mv -f "$FORGE_BIN_DIR/.forge.new" "$FORGE_BIN"
run_binary "$FORGE_BIN" --version > /dev/null \
    || die "the installed binary does not start - run $FORGE_BIN directly for the loader's error"
rm -rf "$STAGING_ROOT"

log_success "forge installed at $FORGE_BIN"
log_success "  transcribe/ggml libraries shipped beside it"
log_success "  forge @ $CURRENT_SHA (branch: $CURRENT_BRANCH, build: $BUILD_LABEL)"

# The browser host: its own signed bundle beside forge's machine-local state.
# `cargo install` builds only the server binary, so the host executable is
# built here and handed to the script that owns the bundle's layout.
# FORGE_INSTALL_BROWSER=0 skips it: the bundle and its driver cost a release
# build and a 140 MB download, which a server-only install does not need.
if [ "$(uname -s)" = "Darwin" ] && [ "${FORGE_INSTALL_BROWSER:-1}" != "0" ]; then
    log_info "Building and staging the browser host bundle"
    (
        cd "$FORGE_REPO"
        cargo build --release --locked -p forge-browser --bin forge-browser
    )
    "$FORGE_REPO/scripts/stage-browser-host.sh"
    # The driver (node + @playwright/mcp) beside the host, where the server
    # reads it. Idempotent at the vendoring's pins, so a reinstall with the
    # stack already in place touches nothing.
    log_info "Vendoring the browser driver stack"
    FORGE_BROWSER_STACK_DIR="$HOME/Library/Application Support/forge/browser/stack" \
        "$FORGE_REPO/scripts/vendor_browser_stack.sh"
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

