#!/usr/bin/env bash
# Run the gate's steps one at a time, in `check_steps` order, stopping at the
# first failure. `check` runs the same steps in four groups through
# `check_groups.sh`; this is the debugging shape.
#
# Usage: check_serial.sh <just> <justfile> <steps>
#
# The machine-wide lock is not this script's: `check` and `check-serial` both
# run through `check_lock.sh run`, so one release trap covers both.
#
# Fail-fast and exit-code preserving: the failing step's own status is what
# this script exits with, and the verdict names the failing step and the
# steps after it that it did not reach.

set -euo pipefail

just="$1"
justfile="$2"
steps_spec="$3"

read -ra steps <<< "$steps_spec"
verdict=""

on_exit() {
    if [ -z "$verdict" ]; then
        echo "[ERROR] check: no verdict, the run ended early"
    fi
}
trap on_exit EXIT

for i in "${!steps[@]}"; do
    step="${steps[$i]}"
    echo "[..] check: $step"
    status=0
    "$just" --justfile "$justfile" "$step" || status=$?
    if [ "$status" -ne 0 ]; then
        later="${steps[*]:i+1}"
        if [ -n "$later" ]; then
            verdict="[ERROR] check: $step failed; not run: $later"
        else
            verdict="[ERROR] check: $step failed"
        fi
        echo "$verdict"
        exit "$status"
    fi
done

verdict="[OK] check: all green (${steps[*]})"
echo "$verdict"
