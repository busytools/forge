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
clippy:
    cargo clippy --all-targets -- -D warnings

# Run the forge-sdk test suite via nextest.
test:
    cargo nextest run -p forge-sdk

# Run tests across the whole workspace (includes forge-test-harness replay).
test-all:
    cargo nextest run

# Run both wire-conformance replays (sdk_wire + daemon_wire) against every
# committed baseline.
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

# Live-capture a single daemon-wire conformance scenario against a real
# forge-daemon. Promote captured trace into
# `baselines/daemon/<VERSION>/<scenario>.jsonl` to lock the baseline.
conformance-capture-daemon test:
    FORGE_DAEMON_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
        --no-capture --run-ignored only daemon_{{test}}

# Build docs with warnings as errors (matches CI's `cargo doc --workspace`
# step so a broken intra-doc link in any workspace crate fails locally,
# not only on CI).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Full pre-commit / pre-PR verification loop.
check: fmt-check clippy test-all doc
