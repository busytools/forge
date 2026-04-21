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

# Run tests across the whole workspace.
test-all:
    cargo nextest run

# Build docs with warnings as errors (matches the parity-check ritual).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p forge-sdk

# Full pre-commit / pre-PR verification loop.
check: fmt-check clippy test doc
