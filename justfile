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

# Run tests across the whole workspace (includes forge-conformance replay).
test-all:
    cargo nextest run

# Run the wire-conformance replay against every committed baseline.
conformance:
    cargo nextest run -p forge-conformance

# Live-capture a single conformance scenario against the real CLI.
# Usage: `just conformance-capture wire_capture_trivial_prompt`
# Burns API tokens. Baseline goes to target/wire-traces/; promote with
# `cp target/wire-traces/capture-<scenario>-<ts>.jsonl \
#    crates/forge-conformance/baselines/<VERSION>/<scenario>.jsonl`.
conformance-capture test:
    FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-conformance \
        --no-capture --run-ignored only {{test}}

# Build docs with warnings as errors (matches the parity-check ritual).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p forge-sdk

# Full pre-commit / pre-PR verification loop.
check: fmt-check clippy test-all doc
