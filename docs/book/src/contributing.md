# Contributing

The full contributor guide lives in
[`CONTRIBUTING.md`](https://github.com/busytools/forge/blob/main/CONTRIBUTING.md)
at the repository root. This page is the short version and the
pointers.

## The gate

```bash
just check
just check-feature-configs
```

`cargo fmt --check`, the Unicode punctuation gate, the scripts' own tests,
the shell crate's rustfmt
check (`client-tauri-fmt`), the client's
Prettier check, ESLint, `svelte-check` and
`tsc --noEmit` and then its vitest run, the shell crate's clippy and tests
(`client-tauri-clippy`, `client-tauri-test`), its android
warnings check (`client-android-warnings`),
clippy with warnings denied, `cargo nextest run --workspace --all-features`,
`cargo test --doc --workspace --all-features`, and `cargo doc`. CI's set
minus its `cargo check --release` job, and plus the
android warnings check CI cannot run yet (`client-android-warnings`; it
needs the Android NDK's target installed - #1959, closed as an enhancement
and tracked in #2258).

The gate is two commands: `just check`, and `just check-feature-configs`,
which compiles the configuration `install.sh` builds. Both green before
you open a pull request. `just check` runs its four independent groups
concurrently - the fast checks, the Rust workspace, the client web steps
and the shell crate's steps - capped by `FORGE_CHECK_JOBS` (default 4; set
it lower on a smaller machine). Every group runs to its own first failure,
a failing group's log tail is printed, and the verdict names the first
failing step; each run's per-step logs stay in their own
`target/check-logs/run.XXXXXX/` directory, printed as `logs: ...` before
the verdict.
`just check-serial` is the same gate one step at a time, for debugging.
When `sccache` is on `PATH`, the recipes export `RUSTC_WRAPPER=sccache` so
compiled objects are shared across worktrees and branches; without it they
build normally.

For a review round, `just check-delta <crate>...` runs the fast slice on
the crates a change touches: their rustfmt check, the Unicode gate, both
clippy feature sets and nextest, plus the client's steps or the shell
crate's clippy and tests when `client` or `client-tauri` is named. It does
not replace the gate; the final head runs the full two commands.

CI runs each build and test job in `ci.yml` only for the paths it serves,
and one job, `ci-required`, always runs, depends on every job in that
workflow, and fails when a job that ran failed or when a job whose paths
changed was skipped. The ruleset requires two checks: that aggregate and
`build site`, the docs workflow's build, which runs on every pull request.

A fresh worktree has no `client/node_modules`; each client step installs it
first when it is missing or older than `package-lock.json`, so the first
`just check` there runs whole rather than stopping at the Prettier step.

The shell under `client/src-tauri/` is its own workspace root, so the
workspace's cargo steps do not reach it; in CI its clippy and tests are the
`client-tauri` job's, and its rustfmt check the `cargo fmt --check` job's
second invocation. `just check`
reaches it through `client-tauri-fmt` (the shell crate's own `cargo fmt
--check`), the crate's own clippy and tests (`client-tauri-clippy` and
`client-tauri-test`) and `client-android-warnings` (the crate compiled for
`aarch64-linux-android` with warnings denied, which is what catches
desktop-only code going dead on Android); the Unicode punctuation gate and
the client's Prettier step reach it too.
`just client-tauri-check` builds the shipping configuration (and runs the
tests) and `just client-tauri-bundle` adds the bundles.
`just client-android-check`
compiles the Kotlin half and runs its unit tests, because nothing else
reaches that code; it needs the Android SDK and a JDK.

The last line it prints is its verdict, `[OK] check: ...` or
`[ERROR] check: <step> failed`, or `[ERROR] check: group reported nothing
(<group>)` when a group died without a report. A failing group stops at
its own first failure; the other groups still run; every failing group's
log tail is printed before the verdict; and the verdict names the first
failing step and the later steps in that group that did not run. Read that line rather
than a pipeline's exit status: piping `just check` through `tail` reports
tail's status, not the recipe's.

## The book's own gate

`docs/book/ui-word-count.sh` holds every `docs/book/src/ui/` page under
600 words of prose outside mockups and collapsed blocks (`--strict`
also counts the landing page's card text). Run it after editing a
surface page.

## The rules that bite first

- **Clippy runs at `pedantic`, denied.** `unwrap`, `expect`, `panic`,
  `exit`, `todo` and `unimplemented` are denied, and `unsafe_code` is
  forbidden workspace-wide. `clippy.toml` relaxes `unwrap`, `expect`
  and `panic` in tests; the other three stay denied everywhere.
- **No `mod.rs`.** `foo.rs` sits next to `foo/`.
- **`WARN` and `ERROR` are for forge's own problems.** A session's own
  work, its tool call failing or its command exiting non-zero, is
  information about that session and belongs at `debug`. A line that
  does claim a problem names the session, the org, the model, the
  account or the path, whichever apply, and carries an `event_name`.
- **No em-dashes, en-dashes, horizontal bars or curly quotes** across
  the scanned source, docs and config file types. CI rejects them.
  Ellipsis is allowed. `just unicode-punct-check` shows what would be
  flagged.
- **New wire surface ships with a captured baseline.** See
  [the wire contract](./wire-contract.md).
- **UI changes update the [UI surface pages](./ui/client/index.md) in the
  same pull request.**
- **Changes that make a page here false update it in the same pull
  request.** This site deploys on every push to `main`, so a page left
  behind is republished wrong within minutes of the merge.

## Where does my code go?

The [architecture page](./architecture.md) has the placement guide.
Bias toward the deeper crate when unsure.

## Commits and pull requests

Short imperative subjects saying what the commit does. One logical unit
per commit. Version bumps in their own commit. Pull request
descriptions are prose, as short as the change allows, linking the
issue they close.
