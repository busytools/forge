# forge workspace task runner.
# All recipes are thin wrappers over cargo; see CLAUDE.md for policy.

# Default: show the available recipes.
default:
    @just --list

# Format check (no writes).
fmt-check:
    cargo fmt --check

# Ellipsis U+2026 is allowed. Test baselines + reference-captures are
# excluded. See the script for the full rationale + the `\u{2014}`
# escape recipe when a codepoint is functionally required (render
# glyph).
#
# Forbid em-dash / en-dash / horizontal-bar / curly quotes in forge-authored source.
unicode-punct-check:
    ./scripts/check_no_unicode_punctuation.sh

# Rewrite files to match rustfmt.
fmt:
    cargo fmt

# Mirrors CI's two clippy invocations. `RUSTFLAGS=-D warnings` matches
# CI's workflow-level env (#257); the `-- -D warnings` after the
# dash-dash applies to clippy's own lints and the env covers everything
# else cargo invokes (rustc compile warnings on test/example/bin targets
# that clippy might let through).
#
# Both feature sets run because neither covers the other: without
# `--all-features` nothing compiles the perf-gated module, and with it
# nothing compiles the `cfg(not(feature = ...))` branches. Every other
# job in CI is already on the flag, so the bare invocation is the only
# thing keeping the default build linted at all.
#
# Lint everything (lib, tests, examples, bins) with warnings as errors.
clippy:
    RUSTFLAGS="-D warnings" cargo clippy --all-targets --workspace -- -D warnings
    RUSTFLAGS="-D warnings" cargo clippy --all-targets --workspace --all-features -- -D warnings

# `RUSTFLAGS=-D warnings` mirrors CI; without it the test-target
# compile is less strict than CI and an unused-import in a test mod
# would pass local but fail CI.
#
# Run the forge-sdk test suite via nextest.
test:
    RUSTFLAGS="-D warnings" cargo nextest run -p forge-sdk

# Mirrors CI's `cargo nextest run --workspace --all-features` so feature-
# gated test mods that CI runs aren't silently skipped locally.
# `RUSTFLAGS=-D warnings` mirrors CI's workflow-level env (#257).
#
# Run tests across the whole workspace (includes forge-test-harness replay).
test-all:
    RUSTFLAGS="-D warnings" cargo nextest run --workspace --all-features

# `RUSTFLAGS=-D warnings` mirrors CI's workflow-level env (#257).
#
# Run wire-conformance replays against every committed baseline.
conformance:
    RUSTFLAGS="-D warnings" cargo nextest run -p forge-test-harness

# Usage: `just conformance-capture-sdk wire_capture_trivial_prompt`
# Burns API tokens. Baseline goes to target/wire-traces/; promote with
# `cp target/wire-traces/capture-<scenario>-<ts>.jsonl \
#    crates/forge-test-harness/baselines/sdk/<VERSION>/<scenario>.jsonl`.
#
# The argument is a nextest test name, which is matched WITHOUT the
# `sdk_` binary-id prefix - nextest filters on the test name alone, so
# any prefixed filter selects nothing. `--no-tests=fail` is explicit
# rather than left to nextest's `auto` default: a typo in the argument
# must not report a clean run, and that default is version-dependent
# and overridable via NEXTEST_NO_TESTS.
#
# An empty argument is rejected rather than passed through: with no
# filter left, the command selects every live-capture scenario and runs
# all of them against the real API.
#
# Live-capture one SDK-wire conformance scenario against the real CLI (burns tokens).
conformance-capture-sdk test:
    @if [ -z "{{test}}" ]; then \
        echo "[ERROR] test name required, e.g. wire_capture_trivial_prompt" >&2; \
        echo "        an empty name captures every scenario for real money" >&2; \
        exit 1; \
    fi
    FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
        --no-capture --run-ignored only --no-tests=fail {{test}}

# Mirrors CI's `cargo doc --workspace --no-deps --all-features`.
# `RUSTDOCFLAGS=-D warnings` denies rustdoc lints (broken links,
# private intra-doc links, etc.); `RUSTFLAGS=-D warnings` mirrors
# CI's workflow-level env so the underlying compile that `cargo doc`
# drives is also strict (#257).
#
# Build docs with warnings as errors.
doc:
    RUSTDOCFLAGS="-D warnings" RUSTFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Deliberately not part of `check`: the only errors it finds are ones
# dev cannot see (`cfg(debug_assertions)`, and anything reachable only
# through `debug_assert!`), and a second full compile is too slow for
# the inner loop. `release` gates on it instead, which is where the
# ordering actually bites.
#
# `--all-features` is what makes this cover the shipped binary rather
# than a configuration nobody installs: install.sh builds with `perf`
# on, and the 18 feature gates behind it are release-compiled nowhere
# else. Without the flag this check passes on a perf-gated release
# break, measured.
#
# Compile the workspace in release. Mirrors CI's `cargo check --release`.
check-release:
    RUSTFLAGS="-D warnings" cargo check --release --workspace --all-targets --all-features

# Full pre-commit / pre-PR verification loop.
check: fmt-check unicode-punct-check clippy test-all doc

# Deliberately not a bare `gh run watch`. Piping it masks the exit code
# AND truncates the log, losing both signals to one pipe - the failure
# this exists to prevent, which has bitten twice.
#
# The verdict line also lands in `target/ci-watch-verdict`, which is the
# path a scripted caller should read. stdout is not a channel this recipe
# controls: a caller piping it through `tail -N` cuts the verdict off the
# end, and across ten audited invocations that lost the verdict three
# times, more often than a swallowed exit code did.
#
# The verdict comes from `gh run view`, not from the watch's exit code.
# That code is not known to be wrong: measured against a finished run it
# is 1 for cancelled and 0 for success. Reading the run's own status is
# simply authoritative whatever the watch does, including exiting early
# without a verdict, which is why this stays correct even if the above
# turns out not to hold everywhere.
#
# headSha is checked before watching, so a superseded run fails in under
# a second instead of after a full test suite.
#
# The run is resolved by workflow, not by recency. A push fires both CI
# and docs, `--limit 1` returns whichever of the two the API happens to
# list first, and because both built the same sha the headSha check
# below waves the wrong one through - a real verdict for the wrong
# workflow.
#
# Watch CI to completion and report the real verdict; optional run id.
ci-watch run_id="":
    #!/usr/bin/env bash
    set -euo pipefail

    # Truncated up front, so the file never hands back a previous
    # invocation's verdict, and written on the paths that reach a
    # verdict. An abort before one exists leaves it empty, not stale.
    verdict_file="target/ci-watch-verdict"
    mkdir -p target
    : > "$verdict_file"
    record() { printf '%s\n' "$1" > "$verdict_file"; }

    branch=$(git rev-parse --abbrev-ref HEAD)
    want_sha=$(git rev-parse HEAD)

    run_id="{{run_id}}"
    if [ -z "$run_id" ]; then
        run_id=$(gh run list --branch "$branch" --workflow ci.yml --limit 1 \
            --json databaseId --jq '.[0].databaseId // empty')
    fi
    if [ -z "$run_id" ]; then
        line="[ERROR] no CI run found for branch $branch"
        record "$line"; echo "$line" >&2
        exit 1
    fi

    # Before watching, not after: waiting out six minutes of nextest to be
    # told the run was never yours is the one case where waiting is
    # guaranteed pointless. Push, watch, push again and the id resolved
    # above is already the superseded one.
    head_sha=$(gh run view "$run_id" --json headSha --jq '.headSha')
    if [ "$head_sha" != "$want_sha" ]; then
        line="[ERROR] run $run_id built $head_sha, not local HEAD $want_sha"
        record "$line"; echo "$line" >&2
        exit 1
    fi

    log=$(mktemp "${TMPDIR:-/tmp}/ci-watch.XXXXXX")
    echo "[..] run $run_id on $branch, log: $log"
    gh run watch "$run_id" --exit-status > "$log" 2>&1 || true

    verdict=$(gh run view "$run_id" --json status,conclusion \
        --jq '"\(.status) \(.conclusion // "none")"')
    read -r status conclusion <<< "$verdict"

    if [ "$status" != "completed" ] || [ "$conclusion" != "success" ]; then
        line="[ERROR] run $run_id: status=$status conclusion=$conclusion"
        record "$line"; echo "$line" >&2
        tail -30 "$log" >&2
        exit 1
    fi

    rm -f "$log"
    line="[OK] run $run_id: success at $head_sha"
    record "$line"; echo "$line"

# Build forge-tui from this checkout into ~/.cargo/bin/forge (release+perf, then zsh completions).
install:
    ./scripts/install.sh

# Only useful for measuring whether perf adds detectable overhead.
#
# Same as `install` but strips the perf sidecar entirely.
install-no-perf:
    ./scripts/install.sh --no-perf

# Untrust forge's retired proxy CA and delete its key material (idempotent, one-shot).
remove-cert:
    ./scripts/remove-cert.sh

# Does NOT push - that's gated per CLAUDE.md and stays explicit.
# Requires cargo-edit (`cargo install cargo-edit`) for `cargo set-version`.
# Gates on check-release because the ordering is what turns a caught
# error into a public one: `cargo install` builds release, and it runs
# after this recipe has already tagged.
# Usage: `just release 0.17.0`
#
# Cut a release: bump the workspace version, commit, tag.
release version: check-release
    @if ! cargo set-version --help >/dev/null 2>&1; then \
        echo "[ERROR] cargo set-version not available - run: cargo install cargo-edit" >&2; \
        exit 1; \
    fi
    @if [ -n "$(git status --porcelain)" ]; then \
        echo "[ERROR] working tree dirty - commit / stash before releasing" >&2; \
        exit 1; \
    fi
    @if [ "$(git rev-parse --abbrev-ref HEAD)" != "main" ]; then \
        echo "[ERROR] not on main - release tags should be cut from main" >&2; \
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
