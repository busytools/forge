# forge workspace task runner.
# Most recipes are thin wrappers over cargo; see CLAUDE.md for policy.

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

# `cargo nextest run` executes no doctest and `cargo check --all-targets`
# builds none, so without this step the Rust fences under `crates/*/src/`
# are compiled by nothing. `forge-sdk`'s minimal example is the only one
# today, and it is what that crate's published rustdoc opens with.
# `--workspace` rather than `-p forge-sdk` so the next crate to grow one is
# covered too. Mirrors CI's step, which sits in the nextest job so the
# dev-profile build it needs is warm; `RUSTFLAGS=-D warnings` mirrors CI's
# workflow-level env (#257).
#
# No `RUSTDOCFLAGS`, and not by omission: rustdoc compiles the doctest and
# `RUSTFLAGS` does not reach it, while `RUSTDOCFLAGS` does and still does
# not fail the compile on a lint - a doctest carrying a `non_snake_case`
# function passes under it, where the same code under rustc does not.
#
# Compile the workspace's doctests (`no_run` fences compile without running).
doctest:
    RUSTFLAGS="-D warnings" cargo test --doc --workspace --all-features

# `RUSTFLAGS=-D warnings` mirrors CI's workflow-level env (#257).
#
# Run wire-conformance replays against every committed baseline.
conformance:
    RUSTFLAGS="-D warnings" cargo nextest run -p forge-test-harness

# Rewrite the socket contract records from the current code. Run
# deliberately, then READ the diff against the client before committing
# it: what this writes is what the encoder currently emits, and a record
# generated blind pins a wrong shape exactly as well as a right one.
#
# A field that moved here is a page that draws blank, so the question the
# diff answers is "which client read follows this", not "does this look
# plausible".
conformance-record-socket:
    RUSTFLAGS="-D warnings" cargo nextest run -p forge-test-harness --test socket_replay \
        --no-tests=fail --run-ignored ignored-only write_the_socket_records

# Runs the 15-clip fixture corpus through the whole dictation pipeline and
# rewrites `crates/forge-dictate/bench/<machine>.toml`. Commit the result:
# the file is the trend, and its diff is the signal.
#
# NEVER FETCHES. Absent weights fail with a message rather than pulling 3 GB,
# because a benchmark - or a cron - that quietly downloads that much is a
# surprise. Run `prepare()` first if it complains.
#
# Non-interactive and roughly 5 seconds, so a scheduled run is cheap. Each
# figure is held to a deadband measured from 14 runs on an unchanged tree, so
# an unchanged pipeline leaves the file byte-identical and there is nothing
# to commit. The raw unsettled numbers print on every run regardless - the
# file is the trend, stdout is the truth.
#
# Benchmark the corpus and rewrite this machine's results file.
bench:
    RUSTFLAGS="-D warnings" cargo nextest run -p forge-dictate --test bench --run-ignored all --no-capture

# Mutation-test the client with Stryker: break each line the tests claim to
# cover and see whether anything fails. `vitest` reports what ran and never
# whether a passing assertion discriminates, so this is the client's only
# instrument for a test that passes for the wrong reason.
#
# Not part of `check`, and deliberately a recipe rather than a CI job: a run
# is minutes and belongs to whoever asks for it, not to every push.
#
# The target set is the client's logic modules, listed in
# `client/stryker.conf.json`; a run reports a score per file and lists every
# mutant that survived, which is a list of assertions that look like they test
# something and do not. Mutating the Svelte components is out on purpose: a
# full render per mutant finds much less than it costs.
#
# This drives Stryker's `command` runner rather than the vitest runner, and
# the reason is worth keeping: the vitest runner filters a mutant's tests by a
# name it joins with a space, where vitest 5 matches its " > " joined full
# test name, so every nested test is skipped and every covered mutant reports
# Survived. Fixes for it exist upstream (#6214, #6220) but are unreleased, and
# the command runner has no such filter. Its numbers were checked against a
# hand-patched runner on this client: `src/composer/meter.ts` 100%,
# `src/chat/report.ts` 68%.
#
# A full-suite command cannot run in the sandbox: nine test files read outside
# the client tree (crates/, docs/) and fail there. The command therefore runs
# `./node_modules/.bin/vitest related` over the same twelve files the `mutate`
# list names - keep the two lists in step, or a file runs against tests that
# never import it and its survivors are false.
#
# Cost is about 2 seconds a mutant at the default concurrency, so
# `just mutate src/composer/meter.ts` is roughly a minute and the whole
# 4,081-mutant set is hours. Run it per module, not per handover.
#
# The sandbox is `client/.stryker-tmp/sandbox-*/`, and a run in flight is a
# second copy of every test file inside the tree - so do not run `just check`
# or `vitest` beside one (vitest collects both, 68 files becoming 136). A
# successful run deletes it; a crashed one leaves it, and `.gitignore`,
# `.prettierignore` and `eslint.config.js` all ignore it.
#
# Usage: `just mutate` for the configured set, or `just mutate src/chat/units.ts`
# for one file or glob.
mutate target="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{target}}" ]; then
        npm --prefix client run mutate
    else
        npm --prefix client run mutate -- --mutate "{{target}}"
    fi

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
        --no-capture --run-ignored only --no-tests=fail -P capture {{test}}

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
# `--all-features` is what gives the `perf` feature gates a release
# compile; they are built nowhere else and this check passes on a
# perf-gated release break without the flag, measured. It does NOT
# stand in for the shipped configuration - it turns `test-helpers` on,
# which `scripts/install.sh` leaves off. That is `check-feature-configs`.
#
# Compile the workspace in release. Mirrors CI's `cargo check --release`.
check-release:
    RUSTFLAGS="-D warnings" cargo check --release --workspace --all-targets --all-features

# Compiles the two forge-tui configurations nothing else builds. The
# install one compiles what `scripts/install.sh` does - the `forge`
# bin, release, `perf` on, the test-only features off, `--locked` - so
# production code reaching a `#[cfg(feature = "test-helpers")]`
# constructor fails here instead of at the next `just install`. The
# `testing` one is that feature by itself: only the dev-dependency
# self-ref ever turns it on, and that turns `test-helpers` on
# alongside, so a `testing` feature that fails to enable what its own
# gated code needs goes unnoticed.
#
# `--all-features` covers neither: it turns every feature on regardless,
# so it never exercises which configuration enables what - the install
# build leaving `test-helpers` off, or `testing` having to forward it.
#
# The client's four steps, cheapest first, so a red never means two things at
# once. They are inside the gate rather than beside it: the client's build,
# typecheck and tests used to be a CI job alone, and a gate that does not run
# reports what a gate that passes does.
#
# A Rust-only change pays them too, roughly half a minute on a gate that
# already runs for minutes.
client-format:
    npm --prefix client run format:check

client-lint:
    npm --prefix client run lint

# The markup tsc never reads, then the TypeScript tree.
client-typecheck:
    npm --prefix client run typecheck:markup
    npm --prefix client run typecheck

client-test:
    npm --prefix client run test

# Compile the desktop shell in the configuration that ships. This is the routine
# gate for anything under client/src-tauri/, and it is quiet.
#
# `--no-bundle` is what makes it quiet, and the CLI build is what makes it mean
# anything: the CLI is what turns off the `dev` cfg, and a `dev` build never
# resolves `frontendDist` at all. The profile is not the switch - `cargo build`
# and `cargo build --release` both exit 0 with `client/dist` deleted and neither
# binary carries an asset path, while this one does, so the embedded assets are
# the thing to check. It also runs `beforeBuildCommand` and validates the
# identifier. What it does NOT cover is the bundle itself: see
# `client-tauri-bundle`, and do not fold it back in here.
#
# `--locked` stops it rewriting the tracked `Cargo.lock` on a manifest edit.
#
# `--ci` is passed explicitly because the CLI reads `CI` from the environment as
# this same boolean flag, and a `CI` holding anything but `true` or `false` -
# `0` is the common one, and forge's own sessions carry it - is rejected before
# the build starts.
#
# Not a step in `check`: the webview compile is minutes and forge runs every
# worker in its own worktree, so it would charge each Rust-only change for a
# crate it never touched. A cost choice rather than an impossibility.
client-tauri-check:
    npm --prefix client run tauri -- build --no-bundle --ci -- --locked

# Build the shell's bundles: `forge.app` and the dmg, under
# client/src-tauri/target/release/bundle. `client-tauri-check` covers no
# bundle at all and `client-release` covers the app alone, so this is the one
# that covers the dmg - run it deliberately when the icon or the dmg's own
# layout changes.
#
# The dmg step mounts the image and runs AppleScript to lay the mounted volume
# out, so it opens a Finder window and takes focus for a few seconds.
#
# **A window on every run is why this is not the routine gate.**
client-tauri-bundle:
    npm --prefix client run tauri -- build --ci -- --locked

# Bundle the client as an app and install it over /Applications/forge.app.
# This is `release`'s last step and does not bump anything, so it can be
# re-run after a failed build - while no source has changed since the tag,
# which is what records the tree being shipped.
#
# `--bundles app` overrides the config's `bundle.targets`, so the dmg target -
# the one that mounts the image and opens a Finder window - is never invoked.
#
# A client already running from that bundle is refused rather than replaced:
# overwriting the bundle a live process is executing out of is the one way
# this fails quietly. The check is on the bundle's own executable, so a client
# running from a checkout's target dir does not block a release - it goes on
# executing the image it started from, which this neither disturbs nor updates.
#
# The version is read back off the built app rather than assumed, because a
# stale bundle from an earlier build installs exactly as quietly as a fresh one.
#
# Install the client: app-only bundle, refused while in use, swapped in.
client-release version:
    #!/usr/bin/env bash
    set -euo pipefail

    app=/Applications/forge.app
    built=client/src-tauri/target/release/bundle/macos/forge.app

    refuse_if_in_use() {
        # Fails closed: without lsof the check cannot answer, and proceeding
        # is the failure it exists to prevent.
        command -v lsof >/dev/null 2>&1 || {
            echo "[ERROR] lsof not found - cannot tell whether the client is running" >&2
            exit 1
        }
        # No bundle, nothing in use - and lsof prints its banner and usage for
        # a path it cannot find.
        [ -e "$app/Contents/MacOS/forge-client" ] || return 0
        if pids=$(lsof -t "$app/Contents/MacOS/forge-client"); then
            echo "[ERROR] $app is in use (pid $pids) - quit the client before releasing" >&2
            echo "        then run: just client-release {{version}}" >&2
            exit 1
        fi
    }

    # Before the build as well as before the swap: the build is minutes, and a
    # client started inside that window would be replaced just as quietly.
    refuse_if_in_use

    npm --prefix client run tauri -- build --bundles app --ci -- --locked

    if [ ! -e "$built" ]; then
        echo "[ERROR] the build produced no app bundle at $built" >&2
        exit 1
    fi

    got=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$built/Contents/Info.plist")
    if [ "$got" != "{{version}}" ]; then
        echo "[ERROR] the built client is version $got, expected {{version}}" >&2
        exit 1
    fi

    # Staged beside the target, and the old bundle moved aside rather than
    # deleted, so a failed copy leaves the installed client untouched and the
    # old bundle stays recoverable until the new one is in place. Between the
    # two renames nothing sits at $app for an instant; closing that would mean
    # leaving shell (a rename cannot replace a non-empty directory), and a
    # launch landing in it fails loudly rather than quietly, so it stays named.
    rm -rf "$app.new"
    # A run killed between the renames leaves the only install at $app.old;
    # deleting it here would destroy the client before the new copy exists.
    if [ ! -e "$app" ] && [ -e "$app.old" ]; then
        echo "[WARN] no client at $app; a previous swap left one at $app.old" >&2
    fi
    ditto "$built" "$app.new"

    # Again here, after the copy, so what follows the check is the two renames
    # and not the copy.
    refuse_if_in_use

    if [ -e "$app" ]; then
        mv "$app" "$app.old"
    fi
    if ! mv "$app.new" "$app"; then
        echo "[ERROR] the swap failed; the previous client is at $app.old" >&2
        exit 1
    fi
    rm -rf "$app.old"
    echo "[OK] installed the client: $app is version $got"

# Run the app: the debug webview over the Vite dev server, with a frontend edit
# reloading into the open window. Nothing is installed and no disk image is
# produced - `client-tauri-check` is the one that builds what ships.
#
# No `--ci`, unlike that check: `tauri dev` has no such flag, so the `0` forge's
# own sessions put in `CI` is never read.
#
# Run the app without installing it or building a bundle.
client-dev:
    npm --prefix client run tauri -- dev

# Build the client and serve it where a reviewer's browser can land.
#
# The front end had no equivalent of the Rust side's one command: its strongest
# check is "serve the page and drive it", and that is the step that varies per
# reviewer - two of one day's four front-end reviews could not reach a page at
# all. This is that step, written down once, and it prints the URL because a
# recipe that serves without saying where has moved the guessing, not removed
# it: guessing an address is how a healthy page read as unreachable.
#
# The address is the machine's WireGuard interface, resolved rather than
# hardcoded, and several utun addresses fail rather than pick one - picking
# wrong is the same silence as not printing the URL.
#
# `vite preview` serves the built bundle, so the dev server's stale module
# graph cannot arise, which is the other way a page reads as empty. The
# compositing check it prints covers the first: a virtualised list draws
# nothing until it has been measured, and a display producing no frames never
# measures it.
client-preview:
    #!/usr/bin/env bash
    set -euo pipefail

    addrs=$(/sbin/ifconfig | awk '/^[a-z]/ { iface = $1 } iface ~ /^utun/ && $1 == "inet" { print $2 }')
    case "$addrs" in
        '')
            echo '[ERROR] no utun interface carries an address - is WireGuard up?' >&2
            exit 1
            ;;
        *$'\n'*)
            echo "[ERROR] more than one utun address, so none is chosen: $addrs" >&2
            exit 1
            ;;
    esac

    port=4173
    # Refused rather than raced. This matches a listener on ANY interface, so
    # a loopback-only server refuses a port that would not have blocked the
    # bind to the WireGuard address; the message names the address it found,
    # which is more use than loosening the check.
    if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
        holder=$(lsof -nP -iTCP:"$port" -sTCP:LISTEN | awk 'NR == 2 { print $1" (pid "$2") on "$9 }')
        echo "[ERROR] port $port is already held by $holder - stop that server, or free the port" >&2
        exit 1
    fi

    npm --prefix client run build

    log=$(mktemp "${TMPDIR:-/tmp}/client-preview.XXXXXX")
    npm --prefix client run preview -- --host "$addrs" --port "$port" --strictPort >"$log" 2>&1 &
    preview=$!
    trap 'kill "$preview" 2>/dev/null || true; rm -f "$log"' EXIT INT TERM

    # The URL is printed only once vite itself reports the bind, and its own
    # line is the one artefact nothing else can counterfeit. Neither cheaper
    # signal survives: the PID here is npm's, which outlives a vite that
    # failed to bind, and a port that answers says only that SOMETHING is
    # listening - which is how a server started in the build window got
    # announced by this recipe. Vite's banner is coloured even to a pipe, so
    # the escapes come off before the match.
    ready=''
    for _ in $(seq 60); do
        plain=$(sed $'s/\033\\[[0-9;]*m//g' "$log")
        case "$plain" in
            *"http://$addrs:$port/"*) ready=yes; break ;;
        esac
        kill -0 "$preview" 2>/dev/null || break
        sleep 0.25
    done
    cat "$log"
    if [ -z "$ready" ]; then
        echo "[ERROR] vite never reported a bind on http://$addrs:$port/ - see above" >&2
        wait "$preview" || true
        exit 1
    fi

    echo
    echo "[OK] the built client is served at http://$addrs:$port/"
    echo '     A production bundle, not the dev server, so there is no module graph'
    echo '     to go stale: a page that looks empty is not a stale reload. Open a'
    echo '     fresh tab and re-check before reading anything as broken.'
    echo
    echo '     Then ask whether the page is compositing. A display producing no'
    echo '     frames draws nothing, and a virtualised list stays empty until a'
    echo '     frame measures it. The line below is one expression: pass it to'
    echo '     browser_evaluate as it stands, or in the console wrap it as'
    echo '     `await (...)()` - on its own it evaluates to the function. It'
    echo '     answers with frames: 5 when the page is drawing; hidden, or 0'
    echo '     frames, means it is not.'
    echo
    echo "() => new Promise(d => { let n = 0, t0 = performance.now(), stop = setTimeout(() => d({ hidden: document.hidden, frames: n, note: 'fewer than 5 frames in 1s' }), 1000); const tick = () => { if (++n === 5) { clearTimeout(stop); d({ hidden: document.hidden, frames: n, ms: Math.round(performance.now() - t0) }); } else requestAnimationFrame(tick); }; requestAnimationFrame(tick); })"

    wait "$preview"

# Compile the feature configurations nothing else builds.
check-feature-configs:
    RUSTFLAGS="-D warnings" cargo check --locked --release -p forge-tui --bin forge --features perf
    RUSTFLAGS="-D warnings" cargo check --locked -p forge-tui --features testing

# Full pre-commit / pre-PR verification loop.
#
# A script rather than a recipe with these as dependencies, because
# a failing dependency aborts just before any recipe body runs, and the
# verdict is printed by the body.
#
# The verdict line is the point. A caller piping this through `tail -N`
# reads the pipe's exit status, not this recipe's, so a red gate can read
# green; leaving the result on the last line means a truncated read still
# carries it. `[no-exit-message]` keeps just's own error line from
# landing after the verdict, and the EXIT trap covers an interrupted run,
# so the last line is a verdict on that path too. That line carries no
# exit status: bash's EXIT trap sees 0 for a signal death, where the
# process's real status is 128 plus the signal.
#
# Fail-fast and exit-code preserving, as the dependency form was: the
# failing step's own status is what this recipe exits with. Both verdict
# lines are built from `steps`, so the list of them has one home.
#
# The steps are re-invoked through just's own executable, not a bare
# `just`, which a caller who invoked it by absolute path does not have on
# PATH.
[no-exit-message]
check:
    #!/usr/bin/env bash
    set -euo pipefail

    steps=(fmt-check unicode-punct-check client-format client-lint client-typecheck client-test clippy test-all doctest doc)
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
        "{{just_executable()}}" --justfile "{{justfile()}}" "$step" || status=$?
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
    # The lookup cannot tell a run that is not registered yet from one that
    # will never exist, so the message says only what it knows.
    if [ -z "$run_id" ]; then
        line="[ERROR] no CI run found for branch $branch yet; GitHub may not have registered it - pass a run id if you just pushed"
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
# Gates on check-release and check-feature-configs because the ordering
# is what turns a caught error into a public one: `cargo install` builds
# release, and it runs after this recipe has already tagged. The second
# is the one that predicts the install - `check-release`'s
# `--all-features` cannot, since it enables the test-only features the
# install build leaves off.
# Usage: `just release 0.17.0`
#
# One number names both halves: this bumps the workspace and the client's
# own manifest to `version`, commits and tags them together, and only then
# builds and installs the client, through `client-release`.
#
# Cut a release: bump the workspace and client versions, commit, tag, install the client.
release version: check-release check-feature-configs
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
    cargo set-version --manifest-path client/src-tauri/Cargo.toml {{version}}
    cargo update --workspace
    git add Cargo.toml Cargo.lock crates/*/Cargo.toml client/src-tauri/Cargo.toml client/src-tauri/Cargo.lock
    git commit -m "release v{{version}}"
    # Annotated (`-m`) so it works under `tag.gpgSign = true`, which
    # forces a signed tag - a bare `git tag <name>` errors with
    # "no tag message?" when signing is on.
    git tag -m "v{{version}}" "v{{version}}"
    "{{just_executable()}}" --justfile "{{justfile()}}" client-release {{version}}
    @echo
    @echo "[OK] released v{{version}}: tagged locally, client installed at /Applications/forge.app"
    @echo "     To publish: git push --follow-tags origin main"
