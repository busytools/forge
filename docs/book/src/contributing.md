# Contributing

The full contributor guide lives in
[`CONTRIBUTING.md`](https://github.com/busytools/forge/blob/main/CONTRIBUTING.md)
at the repository root. This page is the short version and the
pointers.

## The one command

```bash
just check
```

`cargo fmt --check`, the Unicode punctuation gate, clippy with warnings
denied, `cargo nextest run --workspace --all-features`, and
`cargo doc`. CI's set minus its `cargo check --release` job. Green
before you open a pull request.

## The rules that bite first

- **Clippy runs at `pedantic`, denied.** `unwrap`, `expect`, `panic`,
  `exit`, `todo` and `unimplemented` are denied, and `unsafe_code` is
  forbidden workspace-wide. `clippy.toml` relaxes `unwrap`, `expect`
  and `panic` in tests; the other three stay denied everywhere.
- **No `mod.rs`.** `foo.rs` sits next to `foo/`.
- **No em-dashes, en-dashes, horizontal bars or curly quotes** across
  the scanned source and docs file types. CI rejects them. Ellipsis is
  allowed. `just unicode-punct-check` shows what would be flagged.
- **New wire surface ships with a captured baseline.** See
  [the wire contract](./wire-contract.md).
- **UI changes update `docs/forge-map.html` in the same pull request.**
- **Changes that make a page here false update it in the same pull
  request.** This site deploys on every push to `main`, so a page left
  behind is republished wrong within minutes of the merge.

## Where does my code go?

The [architecture page](./architecture.md) has the placement guide.
Bias toward the deeper crate when unsure; the usual mistake is putting
too much in `forge-tui`.

## Commits and pull requests

Short imperative subjects saying what the commit does. One logical unit
per commit. Version bumps in their own commit. Pull request
descriptions are prose, as short as the change allows, linking the
issue they close.
