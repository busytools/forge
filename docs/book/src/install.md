# Install and build

## Prerequisites

- **Rust nightly, at the pinned date.** `rust-toolchain.toml` pins an
  exact nightly channel plus the `rustfmt`, `clippy` and `rust-src`
  components. With `rustup` installed, the pin is applied
  automatically the first time you run a cargo command inside the
  repository, so there is nothing to select by hand.
- **The `claude` CLI.** forge spawns it; it does not bundle it or talk
  to the Anthropic API on its own.
- **`cargo-nextest`**, for the test suite. `just check` uses it rather
  than `cargo test`.
- **`mdbook`**, for the book. CI pins `0.5.4`; rule 11's workflow
  verifies a UI change with `mdbook serve docs/book`.
- **`just`**, if you want the task recipes. Most wrap a cargo
  invocation you can also type out.
- **Node and npm, for the client.** `just check` runs the client's
  Prettier, ESLint, `svelte-check`, `tsc` and vitest steps, and its
  android warnings check (`client-android-warnings`, the shell crate
  compiled for `aarch64-linux-android`), and
  `just client-tauri-check` builds the shell in its shipping configuration;
  `just release` builds and installs it. CI uses Node 24, and
  `npm --prefix client ci` fetches the client's dependencies. The client
  also builds for Android, which additionally wants the Android SDK, its
  NDK and a JDK 17 or newer; the recipe is in `client/README.md`.
- **A C and C++ toolchain, `cmake`, and `libclang`.** `forge-workspace`
  depends on `forge-dictate`, which builds two native model runtimes
  (`transcribe-cpp-sys`, `llama-cpp-sys-2`) - so these are needed to
  build the binary at all, not only to run `--workspace`. `cmake` and a
  C compiler were already required before that, through `aws-lc-sys`;
  the C++ compiler and `libclang` (for `bindgen`) are what the dictation
  runtimes add.

  On macOS: Xcode Command Line Tools cover the compilers and
  `libclang`, and `cmake` comes from Homebrew. On Debian and Ubuntu:
  `build-essential cmake libclang-dev`.
- **ALSA headers, on Linux only.** `cpal`'s Linux backend links against
  ALSA. On Debian and Ubuntu that is `libasound2-dev`; macOS needs no
  audio package.

### About the nightly pin

The pin exists so every developer and CI build use the same compiler,
and CI enforces it rather than trusting it: the toolchain action reads
the channel out of `rust-toolchain.toml`, installs it, and then asserts
that the toolchain rustup will actually select matches the pin. That
guards against a `RUSTUP_TOOLCHAIN` in the environment or a rustup
override on the runner quietly moving CI to a different compiler while
the file still reads as authoritative.

Bump it deliberately, in its own change. No crate in the workspace
opts into an unstable language feature with `#![feature(...)]`, so the
pin buys reproducibility rather than access to any particular nightly
feature. The published rustdoc is the one exception: it needs
`-Z unstable-options --enable-index-page`, which is nightly-only.

## Development is macOS-first

forge is developed and run on macOS. Other platforms compile, and CI
builds and tests on Linux - with one job on macOS that compiles the two
macOS-only halves no Linux runner sees, the server's browser host and the
shell's native overlay, and runs the host's own tests - but one thing
behaves differently:

- **App-level chords use Cmd on macOS, Ctrl elsewhere.** The pane
  toggles (`Cmd+Left`/`Cmd+Right` vs `Ctrl+Left`/`Ctrl+Right`) and
  toggle-all-tool-calls (`Cmd+X` vs `Ctrl+X`) swap their modifier.

## Build and check

```bash
just check
```

That runs four independent groups concurrently: the fast checks
(`cargo fmt --check`, the Unicode punctuation gate, and the scripts' own
tests, `script-tests`: the release manifest's, the client-dependency
bootstrap's and the browser-stack patch's), the Rust workspace
(`cargo clippy --all-targets --workspace -- -D warnings`
once per feature set, with and without `--all-features`,
`cargo nextest run --workspace --all-features`,
`cargo test --doc --workspace --all-features` and
`cargo doc --workspace --no-deps --all-features --document-private-items`),
the client web steps (the client's Prettier check, ESLint, `svelte-check`,
`tsc --noEmit` and then its vitest run), and the shell crate's steps (its
rustfmt check `client-tauri-fmt`, its clippy and nextest run
`client-tauri-clippy` and `client-tauri-test`, and its android warnings
check `client-android-warnings`: `cargo check --target
aarch64-linux-android` with warnings denied). The clippy, test,
doctest and doc steps each set `RUSTFLAGS=-D warnings` so a warning CI
would reject fails locally too; CI sets it once at workflow level
instead. It covers the build each step drives, and rustdoc compiles a
doctest on its own, so it does not deny warnings inside one.

`FORGE_CHECK_JOBS` caps how many of the four groups run at once; it
defaults to 4, so a machine with fewer cores sets it lower
(`FORGE_CHECK_JOBS=2 just check`). Every group runs to its own first
failure, each failing group's log tail is printed, and the verdict names
the first failing step in the gate's order, so a concurrent group cannot
hide one. Each run's per-step logs stay in their own
`target/check-logs/run.XXXXXX/` directory, printed as `logs: ...` before
the verdict.
`just check-serial` is the same gate one step at a time, for debugging a
step or running on a machine that should not carry the groups together.

When `sccache` is on `PATH`, `just` exports `RUSTC_WRAPPER=sccache`, so
compiled objects are shared across worktrees and branches; without it the
recipes build normally. It is never required.

The gate is two commands: run this one before opening a pull request, and
`just check-feature-configs` with it. `just check` runs with
`--all-features`, so it deliberately leaves out the configuration
`install.sh` builds; the second command is what compiles it. Beyond that
this is CI's set with three differences: CI also runs
`cargo check --release` and the client's production build with
`scripts/check_client_wasm.py` (the wasm the highlighter imports, which
`just check` never builds), while `client-android-warnings` (the shell's
android step) is in `just check` alone - the vendoring its build needs
refuses non-macOS hosts, so CI runs no such job yet (issue #1959, closed
as an enhancement and tracked in #2258). The
client's steps are in here too, so the client and the server are not
separate commands, and a failure on either side lands on the same verdict
line.

CI runs each build and test job in `ci.yml` only for the paths it serves,
so a docs-only pull request does not wait for the Rust or client jobs. The
client's CEF job serves every crate in `forge-server`'s dependency graph -
`forge-browser`, `forge-server`, `forge-workspace`, `forge-agent`,
`forge-gateway`, `forge-connectors`, `forge-dictate`, `forge-capture`,
`forge-system-one`, `forge-sdk` and `forge-primitives` - and the inputs
that resolve or build them: `Cargo.lock`, the root `Cargo.toml`,
`rust-toolchain.toml`, `clippy.toml`, `scripts/` and the nextest config.
One job, `ci-required`,
always runs, depends on every job in that workflow, and fails when a job
that ran failed or when a job whose paths changed was skipped. The ruleset
requires two checks: `ci-required` there, and `build site` from the docs
workflow, which runs on every pull request. A skipped job means its paths
did not move, never that the gate passed without looking.

The shell under `client/src-tauri/` is its own workspace root, so the
workspace's cargo steps do not reach it; in CI its clippy and tests are
the `client-tauri` job's, and its rustfmt check the `cargo fmt --check`
job's second invocation. `just check` reaches it through
`client-tauri-fmt` (the shell crate's own `cargo fmt --check`), the
crate's own clippy and tests (`client-tauri-clippy` and
`client-tauri-test`) and
`client-android-warnings` - the crate compiled
for `aarch64-linux-android` with warnings denied, which is what catches
desktop-only code going dead on Android - and the Unicode punctuation gate
and the client's Prettier step reach it too. The shell's shipping
configuration needs `just client-tauri-check` (which runs the tests too)
or `just client-tauri-bundle` (plus the bundles), and the first is the one
to run before handing over a change there.

A client bundles with `just client-tauri-bundle`, and carries no browser:
the engine, its driver and every profile live on the SERVER. The install
(`scripts/install.sh`) is what stages the browser there - a signed
`Forge Browser.app` beside forge's machine-local state, and the driver
stack (`node` and `@playwright/mcp`, about 140 MB) beside it. The vendoring
needs network access the first time and nothing after that, and prints the
pins it verified on every run. **The server's browser is macOS-only**: the
install stages the bundle only on Darwin, and elsewhere its browser tools
answer the no-host refusal by name. **No browser needs installing**: a
client built where the server has no engine answers the browser doors with
the server's own sentence rather than a timeout.

The run ends on a verdict line naming its own result, `[OK] check: ...`
or `[ERROR] check: <step> failed`, or `[ERROR] check: group reported
nothing (<group>)` when a group died without a report. A failing group
stops at its own first failure; the other groups still run; every failing
group's log tail is printed before the verdict; and the verdict names the
first failing step and the later steps in that group that did not run. That
line is deliberately the last one written: piping the output through
`tail` reports tail's exit status rather than the recipe's, so a red gate
reads green to whoever ran it unless the result travels in the text.

Individual pieces, if you want a faster loop:

```bash
just fmt              # rewrite files to match rustfmt
just clippy           # lints only
just test-all         # whole workspace, all features
just test             # forge-sdk only
just conformance      # replay every committed wire baseline
just doctest          # compile the workspace's doctests
just doc              # rustdoc with warnings denied
just check-delta <crate>   # fmt, clippy and nextest for the named crates
just check-serial     # the whole gate, one step at a time
just client-tauri-clippy   # the shell crate's lints
just client-tauri-test     # the shell crate's tests
just client-tauri-check    # the client shell, in the shipping configuration
just client-android-check  # the Kotlin half and its unit tests
just web-image-check       # the web image's poller and serving tests
```

`just check-delta <crate>...` is the fast slice for a review round: each
named workspace crate's rustfmt check, both clippy feature sets and its
nextest run, the Unicode gate, and the client's steps or the shell crate's
clippy and tests when `client` or `client-tauri` is named. It is not the
pre-merge gate: the final head still runs the full `just check` and
`just check-feature-configs` whole.

`just check-release` compiles the workspace in release mode. It is
deliberately not part of `just check`, because a second full compile is
too slow for the inner loop; it catches the errors only a release build
sees.

`just check-feature-configs` compiles the configurations nothing else
builds: the one `just install` produces - release, the test-only features
off, the server binary plus the host executable `install.sh` builds
separately - and `testing` on its own.
`--all-features` cannot stand in for either, because it turns every
feature on regardless and so never exercises which configuration
enables what: the install build leaving `test-helpers` off, or
`testing` having to forward it.

`just release` gates on both.

## Install the binary

```bash
just install
```

This builds `forge` from the current working tree and installs the
binary into cargo's install root, which is `~/.cargo/bin/forge`
unless you have overridden `CARGO_INSTALL_ROOT` or `CARGO_HOME`. It
builds whatever branch is checked out; it does not clone, fetch or
reset.

One side effect worth knowing about: it regenerates the zsh completion
at `~/.zsh/completions/_forge` (override the directory with
`FORGE_ZSH_COMPLETION_DIR`) and removes `~/.zcompdump*` so the next
shell picks it up.

`just release <version>` installs this binary too, and publishes the
release. It bumps the workspace and the client's manifest to the same
number, commits and tags them, then installs the server binary through
this same script, builds and installs the client over
`/Applications/forge.app`, and stages the Android release APK and the web
archive. Only then does it push `main` with the tag and create the GitHub
release with every asset the update path reads; the push and the publish
are last because they are the only irreversible steps. The binary goes
first, so a failure later in the sequence - a client running from the
installed bundle, a missing Android keystore or toolchain - cannot leave
it stale. The install replaces the on-disk binary; a running forge picks
the new one up at its next restart. A failure after the tag aborts the
release with the tag cut; each half re-runs alone with the tree still at
the tag (`just install`, `just client-release <version>`,
`just client-android-release <version>`, `just client-web-release
<version>`), and a release whose tag was cut and pushed finishes with
`just publish <version>`, while re-running `just release` refuses
because the tag exists.

## First run

forge needs a `forge.toml` before it can start. Create
`<config_dir>/forge/forge.toml` with at least one org containing one
project, and at least one account. The
[configuration reference](./configuration.md) has a complete example.

```bash
forge              # open the auto-start projects
forge <PROJECT>    # open a specific project by its forge.toml name
```

Useful flags:

| Flag | Effect |
|---|---|
| `--new` | Start every boot session fresh instead of resuming. Only affects the startup wave. |
| `--log-file <PATH>` | Write tracing diagnostics to a file. |
| `--log-filter <FILTER>` | Tracing filter directives, for example `info,bridge.sdk=trace`. Overrides `--diagnostics-preset`, and falls back to `RUST_LOG` when omitted. |
| `--diagnostics-preset <NAME>` | A named logging preset: `runtime`, `session`, `bridge` or `full`. Ignored when `--log-filter` is given. |
