# forge workspace task runner.
# All recipes are thin wrappers over cargo; see CLAUDE.md for policy.

# Default: show the available recipes.
default:
    @just --list

# Format check (no writes).
fmt-check:
    cargo fmt --check

# Rewrite files to match rustfmt.
fmt:
    cargo fmt

# Lint everything (lib, tests, examples, bins) with warnings as errors.
# Mirrors CI's `cargo clippy --all-targets --workspace -- -D warnings`.
clippy:
    cargo clippy --all-targets --workspace -- -D warnings

# Run the forge-sdk test suite via nextest.
test:
    cargo nextest run -p forge-sdk

# Run tests across the whole workspace (includes forge-test-harness replay).
# Mirrors CI's `cargo nextest run --workspace --all-features` so feature-
# gated test mods that CI runs aren't silently skipped locally.
test-all:
    cargo nextest run --workspace --all-features

# Run wire-conformance replays against every committed baseline.
conformance:
    cargo nextest run -p forge-test-harness

# Live-capture a single SDK-wire conformance scenario against the real CLI.
# Usage: `just conformance-capture-sdk wire_capture_trivial_prompt`
# Burns API tokens. Baseline goes to target/wire-traces/; promote with
# `cp target/wire-traces/capture-<scenario>-<ts>.jsonl \
#    crates/forge-test-harness/baselines/sdk/<VERSION>/<scenario>.jsonl`.
conformance-capture-sdk test:
    FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
        --no-capture --run-ignored only sdk_{{test}}

# Build docs with warnings as errors. Mirrors CI's
# `cargo doc --workspace --no-deps --all-features`.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Full pre-commit / pre-PR verification loop.
check: fmt-check clippy test-all doc

# Build forge-tui from the current checkout and install the `forge`
# binary into ~/.cargo/bin/forge. Defaults to release+perf. Wraps up
# by refreshing the wire-rewriter CA in the System keychain (best-
# effort — repeat installs are no-ops via SHA-256 fingerprint compare).
install:
    ./scripts/install.sh

# Same as `install` but strips the perf sidecar entirely. Only useful
# for measuring whether perf adds detectable overhead.
install-no-perf:
    ./scripts/install.sh --no-perf

# Install / refresh forge's wire-rewriter CA in the System keychain
# without rebuilding the binary. Content-idempotent — a Touch ID
# prompt fires only on first install or after a CA rotation.
install-cert:
    ./scripts/install-cert.sh

# Report-only: is forge's CA currently trusted in the System keychain?
install-cert-status:
    ./scripts/install-cert.sh --status

# Remove forge's CA from the System keychain.
install-cert-uninstall:
    ./scripts/install-cert.sh --uninstall

# Cut a release: bump the workspace version, commit, tag.
# Does NOT push — that's gated per CLAUDE.md and stays explicit.
# Requires cargo-edit (`cargo install cargo-edit`) for `cargo set-version`.
# Usage: `just release 0.17.0`
release version:
    @if ! cargo set-version --help >/dev/null 2>&1; then \
        echo "[ERROR] cargo set-version not available — run: cargo install cargo-edit" >&2; \
        exit 1; \
    fi
    @if [ -n "$(git status --porcelain)" ]; then \
        echo "[ERROR] working tree dirty — commit / stash before releasing" >&2; \
        exit 1; \
    fi
    @if [ "$(git rev-parse --abbrev-ref HEAD)" != "main" ]; then \
        echo "[ERROR] not on main — release tags should be cut from main" >&2; \
        exit 1; \
    fi
    @if git rev-parse "v{{version}}" >/dev/null 2>&1; then \
        echo "[ERROR] tag v{{version}} already exists" >&2; \
        exit 1; \
    fi
    cargo set-version --workspace {{version}}
    cargo update --workspace
    git add Cargo.toml Cargo.lock crates/*/Cargo.toml
    git commit -m "release v{{version}}"
    git tag "v{{version}}"
    @echo
    @echo "[OK] tagged v{{version}} locally. To publish:"
    @echo "     git push --follow-tags origin main"
