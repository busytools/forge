# Contributing to forge

Patches are welcome. This is short on purpose.

## Before you start

forge was written for one person's use and is macOS-first. It is
usable and readable as open source, but it has not been generalised for
arbitrary deployments, and some behaviour is opinionated because it was
only ever meant to serve one workflow. If a change would broaden the
scope rather than fix or improve what is there, open an issue first so
we can agree the direction before you write it.

## Run the checks

```bash
just check
```

That is `cargo fmt --check`, the Unicode punctuation gate,
`cargo clippy --all-targets --workspace -- -D warnings` once per feature
set (with and without `--all-features`),
`cargo nextest run --workspace --all-features`, and
`cargo doc --workspace --no-deps --all-features`, all with
`RUSTFLAGS=-D warnings` on the clippy, test and doc steps. Get it
green before you open a pull request. It is CI's set minus one job:
CI also runs `cargo check --release`.

The toolchain comes from `rust-toolchain.toml`; rustup applies it
automatically. Tests run through `cargo nextest`, not `cargo test`.

## House rules

**Lints.** Clippy runs at `pedantic`, denied. On top of that,
`unwrap`, `expect`, `panic`, `exit`, `todo` and `unimplemented` are
denied, and `unsafe_code` is forbidden workspace-wide. `clippy.toml`
relaxes `unwrap`, `expect` and `panic` in tests; `exit`, `todo` and
`unimplemented` stay denied everywhere. Reach for `Result` rather than
an `#[allow(...)]`; an allow on production code is treated as debt
rather than a fix.

**No `mod.rs`.** A module file sits next to its directory:
`foo.rs` alongside `foo/`.

**Unicode punctuation is gated in CI.** Em-dashes, en-dashes,
horizontal bars and curly quotes are rejected across the source and
docs file types; `scripts/check_no_unicode_punctuation.py` carries the
current list. Use a spaced hyphen, a comma, or two sentences.
Ellipsis is allowed, because the TUI needs it as a truncation glyph.
When a banned codepoint is genuinely required, write the escape form
(`"\u{2014}"`) rather than the literal character. Run
`just unicode-punct-check` to see what it would flag.

**Tests come first where the shape is obvious.** Write the failing
test, watch it fail, implement, watch it pass. For exploratory
refactors, integration coverage is enough. Keep the test count small
and the coverage complete: one test per property, not one per variation
you could construct.

**Comments are for why, not what.** The code says what it does. A
comment earns its place when a future reader would otherwise ask why,
or when it records an external constraint or an API quirk that is not
visible locally.

**New wire surface ships with a captured baseline.** If you add a
control-request subtype, a message type, a hook event or a tool
integration, the same pull request needs a live-capture scenario, the
captured baseline committed under the pinned CLI version's directory,
and a clean replay. See
[the wire contract page](https://busytools.github.io/forge/wire-contract.html)
for the details.

**UI changes update `docs/forge-map.html` in the same pull request.**
That file is the visual record of every surface `forge-tui` can render,
and it is scoped to current state, so it must never describe something
the code does not ship.

**A change that makes a page under `docs/book/` false updates it in the
same pull request.** That directory is the published documentation, and
it deploys on every push to `main`, so a page left behind is
republished wrong within minutes of the merge.

## Commits

Short, imperative subjects that say what the commit does: `fix auth
timeout`, not `fixes for the auth stuff` and not `review feedback`.

One logical unit per commit, each standing alone as something that
could be reviewed or reverted on its own. Do not split a change into
"add the infrastructure" and "use it" when the first half does nothing
by itself, and do not merge unrelated concerns to shrink the count.

Version bumps go in their own commit, separate from the substantive
change.

## Pull requests

Say what the change does and why, in prose, in as few sentences as it
needs. Link the issue it closes. CI has to be green.

Keep the diff to what was discussed. A pull request that is correct but
three times larger than the problem is harder to review than one that
is smaller and slightly incomplete.

## Where code goes

The workspace is nine crates with a strictly acyclic dependency graph,
and putting something in the wrong one is the most common structural
review comment. The
[architecture page](https://busytools.github.io/forge/architecture.html)
has the placement guide; the short version is that audio and speech go
in `forge-dictate`, cross-crate types go in `forge-primitives`,
provider credentials, probes and billing go in `forge-providers`,
inbound connector clients and matching go in `forge-connectors`,
anything speaking stream-json goes in `forge-sdk`, environment and I/O
go in `forge-agent`, multi-session orchestration goes in
`forge-workspace`, and only what the user sees goes in `forge-tui`.
