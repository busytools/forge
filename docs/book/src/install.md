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
- **`just`**, if you want the task recipes. Everything they run is a
  cargo invocation you can also type out.

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
feature.

## Development is macOS-first

forge is developed and run on macOS. Other platforms compile, and CI
builds and tests on Linux, but two things behave differently:

- **OAuth credentials are read from the macOS Keychain.** The reader is
  gated behind `#[cfg(target_os = "macos")]`; on every other target it
  returns nothing, unconditionally. It feeds the boot-time account
  loader and the usage probe, so on other platforms the surfaces built
  on those (account usage, plan tier) have no token to authenticate
  with. An account that sets `ANTHROPIC_BASE_URL` in its
  `[accounts.env]` is unaffected, because that path carries its own
  bearer token and never consults the keychain.

## Build and check

```bash
just check
```

That runs, in order: `cargo fmt --check`, the Unicode punctuation gate,
`cargo clippy --all-targets --workspace -- -D warnings` once per feature
set (with and without `--all-features`),
`cargo nextest run --workspace --all-features`, and
`cargo doc --workspace --no-deps --all-features`. The clippy, test and
doc steps each set `RUSTFLAGS=-D warnings` so a warning CI would reject
fails locally too; CI sets it once at workflow level instead.

Run it before opening a pull request. It is CI's set minus one job: CI
also runs `cargo check --release`, which `just check` deliberately
leaves out.

Individual pieces, if you want a faster loop:

```bash
just fmt              # rewrite files to match rustfmt
just clippy           # lints only
just test-all         # whole workspace, all features
just test             # forge-sdk only
just conformance      # replay every committed wire baseline
just doc              # rustdoc with warnings denied
```

`just check-release` compiles the workspace in release mode. It is
deliberately not part of `just check`, because a second full compile is
too slow for the inner loop; it catches the errors only a release build
sees, and `just release` gates on it.

## Install the binary

```bash
just install
```

This builds `forge-tui` from the current working tree and installs the
`forge` binary into cargo's install root, which is `~/.cargo/bin/forge`
unless you have overridden `CARGO_INSTALL_ROOT` or `CARGO_HOME`. It
builds whatever branch is checked out; it does not clone, fetch or
reset. It passes `--features perf` unless you use
`just install-no-perf`. That default lives in the install script, not
in `Cargo.toml`, so a plain `cargo build` or `cargo install` does not
enable `perf`.

One side effect worth knowing about: it regenerates the zsh completion
at `~/.zsh/completions/_forge` (override the directory with
`FORGE_ZSH_COMPLETION_DIR`) and removes `~/.zcompdump*` so the next
shell picks it up.

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
| `--log-filter <FILTER>` | Tracing filter directives, for example `info,app.render=trace`. Overrides `--diagnostics-preset`, and falls back to `RUST_LOG` when omitted. |
| `--diagnostics-preset <NAME>` | A named logging preset: `runtime`, `session`, `render`, `bridge` or `full`. Ignored when `--log-filter` is given. |
| `--perf-log <PATH>` | Write perf telemetry to a sidecar JSON file. Needs a build with the `perf` feature. |
