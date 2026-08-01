# forge workspace task runner.
# All recipes are thin wrappers over cargo; see CLAUDE.md for policy.

# Default: show the available recipes.
default:
    @just --list

# Format check (no writes).
fmt-check:
    cargo fmt --check

# Forbid em-dash / en-dash / horizontal-bar / curly quotes in
# forge-authored source. Ellipsis U+2026 is allowed. Test baselines
# + reference-captures are excluded. See the script for the full
# rationale + the `\u{2014}` escape recipe when a codepoint is
# functionally required (render glyph).
unicode-punct-check:
    ./scripts/check_no_unicode_punctuation.sh

# Rewrite files to match rustfmt.
fmt:
    cargo fmt

# Lint everything (lib, tests, examples, bins) with warnings as errors.
# Mirrors CI's `cargo clippy --all-targets --workspace -- -D warnings`.
# `RUSTFLAGS=-D warnings` matches CI's workflow-level env (#257); the
# `-- -D warnings` after the dash-dash applies to clippy's own lints
# and the env covers everything else cargo invokes (rustc compile
# warnings on test/example/bin targets that clippy might let through).
clippy:
    RUSTFLAGS="-D warnings" cargo clippy --all-targets --workspace -- -D warnings

# Run the forge-sdk test suite via nextest.
# `RUSTFLAGS=-D warnings` mirrors CI; without it the test-target
# compile is less strict than CI and an unused-import in a test mod
# would pass local but fail CI.
test:
    RUSTFLAGS="-D warnings" cargo nextest run -p forge-sdk

# Run tests across the whole workspace (includes forge-test-harness replay).
# Mirrors CI's `cargo nextest run --workspace --all-features` so feature-
# gated test mods that CI runs aren't silently skipped locally.
# `RUSTFLAGS=-D warnings` mirrors CI's workflow-level env (#257).
test-all:
    RUSTFLAGS="-D warnings" cargo nextest run --workspace --all-features

# Run wire-conformance replays against every committed baseline.
# `RUSTFLAGS=-D warnings` mirrors CI's workflow-level env (#257).
conformance:
    RUSTFLAGS="-D warnings" cargo nextest run -p forge-test-harness

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
# `RUSTDOCFLAGS=-D warnings` denies rustdoc lints (broken links,
# private intra-doc links, etc.); `RUSTFLAGS=-D warnings` mirrors
# CI's workflow-level env so the underlying compile that `cargo doc`
# drives is also strict (#257).
doc:
    RUSTDOCFLAGS="-D warnings" RUSTFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Full pre-commit / pre-PR verification loop.
check: fmt-check unicode-punct-check clippy test-all doc

# Deliberately not a bare `gh run watch`. Piping that masks its exit code
# AND truncates its output, so one pipe loses both signals at once. Its
# exit code is unreliable regardless - 0 on a concurrency-cancelled run,
# and 0 with jobs still in flight after a transient 502 - so the verdict
# is read back separately instead. headSha is compared against local HEAD
# so a superseded earlier run cannot read as a pass for work it never
# built.
#
# Watch CI to completion and report the real verdict; optional run id.
ci-watch run_id="":
    #!/usr/bin/env bash
    set -euo pipefail

    branch=$(git rev-parse --abbrev-ref HEAD)
    want_sha=$(git rev-parse HEAD)

    run_id="{{run_id}}"
    if [ -z "$run_id" ]; then
        run_id=$(gh run list --branch "$branch" --limit 1 \
            --json databaseId --jq '.[0].databaseId // empty')
    fi
    if [ -z "$run_id" ]; then
        echo "[ERROR] no CI run found for branch $branch" >&2
        exit 1
    fi

    log=$(mktemp "${TMPDIR:-/tmp}/ci-watch.XXXXXX")
    echo "[..] run $run_id on $branch, log: $log"
    gh run watch "$run_id" --exit-status > "$log" 2>&1 || true

    verdict=$(gh run view "$run_id" --json status,conclusion,headSha \
        --jq '"\(.status) \(.conclusion // "none") \(.headSha)"')
    read -r status conclusion head_sha <<< "$verdict"

    if [ "$head_sha" != "$want_sha" ]; then
        echo "[ERROR] run $run_id built $head_sha, not local HEAD $want_sha" >&2
        exit 1
    fi
    if [ "$status" != "completed" ] || [ "$conclusion" != "success" ]; then
        echo "[ERROR] run $run_id: status=$status conclusion=$conclusion" >&2
        tail -30 "$log" >&2
        exit 1
    fi

    rm -f "$log"
    echo "[OK] run $run_id: success at $head_sha"

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
    # Annotated (`-m`) so it works under `tag.gpgSign = true`, which
    # forces a signed tag - a bare `git tag <name>` errors with
    # "no tag message?" when signing is on.
    git tag -m "v{{version}}" "v{{version}}"
    @echo
    @echo "[OK] tagged v{{version}} locally. To publish:"
    @echo "     git push --follow-tags origin main"
