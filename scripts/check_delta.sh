#!/usr/bin/env bash
# Run the fast review-round slice: fmt, the Unicode gate, both clippy feature
# sets and nextest for the named crates, plus the client's steps or the shell
# crate's when named.
#
# Usage: check_delta.sh <just> <justfile> <packages>
#
# The machine-wide lock is not this script's: `check-delta` runs through
# `check_lock.sh run`, because the browser live tests are not ignored and a
# slice that names forge-browser boots real CEF hosts.
#
# Ends on `[OK] check-delta: ...` or `[ERROR] check-delta: <step> failed` as
# its last line.

set -uo pipefail

just="$1"
justfile="$2"
read -ra packages <<< "$3" || true
if [ "${#packages[@]}" -eq 0 ]; then
    echo "[ERROR] check-delta: name at least one crate, client or client-tauri" >&2
    exit 1
fi

verdict=""

on_exit() {
    if [ -z "$verdict" ]; then
        echo "[ERROR] check-delta: no verdict, the run ended early"
    fi
}
trap on_exit EXIT

run_just() {
    "$just" --justfile "$justfile" "$@"
}

# One failing step stops the run and its name lands on the last line, so a
# caller piping this through `tail` reads the verdict, not tail's status.
run_step() {
    local label="$1"
    shift
    echo "[..] check-delta: $label"
    local status=0
    "$@" || status=$?
    if [ "$status" -ne 0 ]; then
        verdict="[ERROR] check-delta: $label failed"
        echo "$verdict"
        exit "$status"
    fi
}

for package in "${packages[@]}"; do
    case "$package" in
        client) echo "[..] check-delta: client/ (format, lint, typecheck, test)" ;;
        client-tauri) echo "[..] check-delta: client/src-tauri/ (fmt, clippy, test)" ;;
        *) echo "[..] check-delta: $package (fmt, clippy, nextest)" ;;
    esac
done

for package in "${packages[@]}"; do
    case "$package" in
        client) ;;
        client-tauri) run_step "client-tauri-fmt" run_just client-tauri-fmt ;;
        *) run_step "fmt $package" cargo fmt -p "$package" --check ;;
    esac
done

run_step "unicode-punct-check" run_just unicode-punct-check

for package in "${packages[@]}"; do
    case "$package" in
        client)
            run_step "client-format" run_just client-format
            run_step "client-lint" run_just client-lint
            run_step "client-typecheck" run_just client-typecheck
            run_step "client-test" run_just client-test
            ;;
        client-tauri)
            run_step "client-tauri-clippy" run_just client-tauri-clippy
            run_step "client-tauri-test" run_just client-tauri-test
            ;;
        *)
            run_step "clippy $package" env RUSTFLAGS="-D warnings" cargo clippy -p "$package" --all-targets -- -D warnings
            run_step "clippy $package (all features)" env RUSTFLAGS="-D warnings" cargo clippy -p "$package" --all-targets --all-features -- -D warnings
            run_step "nextest $package" env RUSTFLAGS="-D warnings" cargo nextest run -p "$package" --all-features
            ;;
    esac
done

verdict="[OK] check-delta: all green (${packages[*]})"
echo "$verdict"
