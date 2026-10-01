#!/usr/bin/env bash
# Run a cargo command, keeping its output, and fail if it compiled nothing.
#
# A cargo command whose units are all fresh prints nothing and exits 0, so a
# job can report success having checked none of the tree: a run that built
# everything and found no problems is indistinguishable from one that built
# nothing. This makes the second a failure.
#
# Wrap the FIRST cargo invocation of a job. The workspace crates are never
# restored from the CI cache, so it always has work, while a later invocation
# in the same job may legitimately be a no-op against a tree the earlier one
# just built. That same assumption fails on a developer's warm tree, which is
# why this wraps CI's steps and not `just check`'s.
#
# Usage: require_compiled.sh cargo clippy --all-targets --workspace -- -D warnings
set -euo pipefail

log=$(mktemp "${TMPDIR:-/tmp}/require-compiled.XXXXXX")
trap 'rm -f "$log"' EXIT

# `pipefail` turns the command's own status into the pipeline's, so a real
# failure exits here and never reaches the check below.
"$@" 2>&1 | tee "$log"

# CI sets `CARGO_TERM_COLOR: always`, so the verb is followed by an escape
# sequence rather than a space. Left in, the escape defeats the match and the
# check fails on every run - so the escapes come off first.
if ! sed $'s/\033\\[[0-9;]*m//g' "$log" | grep -qE '^[[:space:]]*(Compiling|Checking) [^ ]+ v'; then
    echo "::error::$* compiled no crate - this run checked nothing, which is not a pass" >&2
    exit 1
fi
