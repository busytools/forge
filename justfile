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
