#!/usr/bin/env bash
# Run the gate's groups concurrently and report the first failing step.
#
# Usage: check_groups.sh <just> <justfile> <steps> <groups>
#   steps  - space-delimited step names, in the order a verdict reports them
#   groups - `name|step step;name|step step`, the recipe's `check_groups`
#
# Env:
#   FORGE_CHECK_JOBS   how many groups run at once (default 4)
#   CHECK_LOG_ROOT     where run directories live (default target/check-logs)
#   CHECK_LOG_DIR      exported to each step: the run's own log directory
#                      (the dead-worker test's stub uses it)
#
# Each run gets its own `run.XXXXXX` directory under the log root, and every
# step writes its full output there; the directory is printed with the
# summary. A group's report file is created before its worker starts, so a
# worker killed before it can write one is a failure, not a pass.
#
# Exit status is the first failing step's, or 1 when a group died without a
# report. The verdict is the last line written.

set -uo pipefail

just="$1"
justfile="$2"
steps_spec="$3"
groups_spec="$4"

read -ra steps <<< "$steps_spec"
IFS=';' read -ra specs <<< "$groups_spec"

group_names=()
group_steps=()
grouped=" "
for spec in "${specs[@]}"; do
    group_names+=("${spec%%|*}")
    group_steps+=("${spec#*|}")
    for step in ${spec#*|}; do
        grouped="$grouped$step "
    done
done

# A step added to the step list but to no group would never run, and one in
# two groups would run twice. Refuse before running anything.
expected="$(printf '%s\n' "${steps[@]}" | sort)"
actual="$(printf '%s\n' $grouped | sort)"
if [ "$expected" != "$actual" ]; then
    echo "[ERROR] check: check_steps and check_groups disagree" >&2
    exit 2
fi

max="${FORGE_CHECK_JOBS:-4}"
case "$max" in
    ''|*[!0-9]*)
        echo "[ERROR] check: FORGE_CHECK_JOBS must be a positive integer" >&2
        exit 2
        ;;
esac
if [ "$max" -lt 1 ]; then
    echo "[ERROR] check: FORGE_CHECK_JOBS must be a positive integer" >&2
    exit 2
fi

logroot="${CHECK_LOG_ROOT:-target/check-logs}"
mkdir -p "$logroot" || {
    echo "[ERROR] check: cannot write $logroot" >&2
    exit 2
}
# Keep the newest five run directories. `target/` is disposable, but a gate
# that runs many times a day should not grow it without bound. Pruning runs
# before this run's directory exists, so a concurrent run's newer directory
# is among those kept.
ls -1dt "$logroot"/run.* 2>/dev/null | tail -n +6 | while IFS= read -r old; do
    rm -rf "$old"
done
logdir="$(mktemp -d "$logroot/run.XXXXXX")" || {
    echo "[ERROR] check: cannot write $logroot" >&2
    exit 2
}

verdict=""

on_exit() {
    if [ -z "$verdict" ]; then
        echo "[ERROR] check: no verdict, the run ended early"
    fi
}
trap on_exit EXIT

no_report="__no_report__"

run_group() {
    trap - EXIT
    local name="$1" steps="$2" step status failed=""
    export CHECK_LOG_DIR="$logdir"
    for step in $steps; do
        echo "[..] check: $step"
        status=0
        "$just" --justfile "$justfile" "$step" >"$logdir/$step.log" 2>&1 || status=$?
        if [ "$status" -ne 0 ]; then
            failed="$step"
            printf '%s' "$status" > "$logdir/$name.status"
            break
        fi
    done
    # Written aside and moved into place: a kill between truncate and write
    # would otherwise leave an empty report, which reads as a pass. Until
    # the move lands, the sentinel written before the worker started stands.
    printf '%s' "$failed" > "$logdir/$name.failed.tmp" &&
        mv "$logdir/$name.failed.tmp" "$logdir/$name.failed"
}

pids=()
for i in "${!group_names[@]}"; do
    # The report exists before the worker does: a worker that dies before
    # writing its own is then detected instead of read as a pass.
    printf '%s' "$no_report" > "$logdir/${group_names[$i]}.failed"
    while [ "${#pids[@]}" -ge "$max" ]; do
        # The window retires the oldest child, not the first to finish. At
        # four groups that costs at most one group waiting behind a slow
        # older one, which does not justify a reaper loop here.
        wait "${pids[0]}" || true
        pids=("${pids[@]:1}")
    done
    run_group "${group_names[$i]}" "${group_steps[$i]}" &
    pids+=("$!")
done
wait

failed_any=0
failed_steps=" "
missing_groups=" "
for i in "${!group_names[@]}"; do
    name="${group_names[$i]}"
    report="$logdir/$name.failed"
    # A failed `cat` is a missing or unreadable report, never a pass: a
    # report we cannot read says nothing about the group.
    if ! failed="$(cat "$report" 2>/dev/null)" || [ "$failed" = "$no_report" ]; then
        missing_groups="$missing_groups$name "
        failed_any=1
        continue
    fi
    [ -n "$failed" ] || continue
    failed_any=1
    failed_steps="$failed_steps$failed "
done

echo "logs: $logdir"

if [ "$failed_any" -eq 0 ]; then
    verdict="[OK] check: all green (${steps[*]})"
    echo "$verdict"
    exit 0
fi

for i in "${!group_names[@]}"; do
    name="${group_names[$i]}"
    report="$logdir/$name.failed"
    if ! failed="$(cat "$report" 2>/dev/null)" || [ "$failed" = "$no_report" ]; then
        echo "[ERROR] check: $name group reported nothing"
        echo "---- $name (its steps may not have run: ${group_steps[$i]})"
        echo "----"
        continue
    fi
    [ -n "$failed" ] || continue
    echo "[ERROR] check: $name group failed at $failed"
    echo "---- $failed (last 25 lines; full log: $logdir/$failed.log)"
    tail -n 25 "$logdir/$failed.log"
    echo "----"
done

first_failed=""
for step in "${steps[@]}"; do
    case "$failed_steps" in
        *" $step "*)
            first_failed="$step"
            break
            ;;
    esac
done

if [ -z "$first_failed" ]; then
    missing="${missing_groups# }"
    verdict="[ERROR] check: group reported nothing (${missing% })"
    echo "$verdict"
    exit 1
fi

first_group=""
for i in "${!group_names[@]}"; do
    failed="$(cat "$logdir/${group_names[$i]}.failed" 2>/dev/null || true)"
    if [ "$failed" = "$first_failed" ]; then
        first_group="$i"
        break
    fi
done

later=""
seen=0
for step in ${group_steps[$first_group]}; do
    if [ "$seen" -eq 1 ]; then
        later="$later $step"
    fi
    if [ "$step" = "$first_failed" ]; then
        seen=1
    fi
done

status="$(cat "$logdir/${group_names[$first_group]}.status" 2>/dev/null || true)"
later="${later# }"
if [ -n "$later" ]; then
    verdict="[ERROR] check: $first_failed failed; not run: $later"
else
    verdict="[ERROR] check: $first_failed failed"
fi
echo "$verdict"
exit "${status:-1}"
