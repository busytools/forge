# Parity check runbook

Weekly process for keeping `forge-sdk` aligned with upstream Python
`claude-agent-sdk`. Run every Monday (or first working day of the week).

See also: [`../PARITY.md`](../PARITY.md) for the log of past runs and
the current parity target.

---

## Prerequisites

- `gh` CLI authenticated for `anthropics/claude-agent-sdk-python`.
- Python SDK source cloned or installed in a venv (for diffing).
- Current working tree clean.

## Steps

### 1. Check for new upstream releases

```bash
gh release list --repo anthropics/claude-agent-sdk-python --limit 10
```

Note any releases newer than the `Target Python SDK version` field in
`PARITY.md`.

### 2. Diff source and tests

For each new release `<NEW>` since `<PREV>`:

```bash
gh api repos/anthropics/claude-agent-sdk-python/compare/<PREV>...<NEW> \
  --jq '.files[] | .filename' | sort -u
```

Focus areas:

- `src/claude_agent_sdk/types.py` — public type changes.
- `src/claude_agent_sdk/_internal/**` — wire-protocol changes.
- `src/claude_agent_sdk/_internal/query.py` — control_request /
  control_response shapes.
- `src/claude_agent_sdk/_internal/transport/subprocess_cli.py` — CLI
  argv + MCP config shape.
- `tests/**` — upstream behavioural spec.

### 3. Classify diffs

For each hunk, label:

- **trivial** — field rename, docstring fix, logging tweak. Apply
  directly; no test needed beyond what already exists.
- **behavioural** — changes response shape, adds a new control subtype,
  alters timing. Needs a mirrored test in
  `crates/forge-sdk/tests/python_parity/`.
- **new-public-api** — new method/type exposed. Needs a new Rust
  counterpart with a mirrored test.

### 4. Diff tests separately

Upstream `tests/` is the executable spec. Diff it with:

```bash
gh api repos/anthropics/claude-agent-sdk-python/compare/<PREV>...<NEW> \
  --jq '.files[] | select(.filename | startswith("tests/"))'
```

Every new or changed Python test must translate into a corresponding
Rust test in `crates/forge-sdk/tests/python_parity/` within the same week.

### 5. Open issues

Open one `vedhavyas/forge` issue per non-trivial item. Include:

- Upstream PR or commit link.
- Classification.
- Rough impact estimate (new field / new behaviour / new API / new test).

### 6. Land the changes

Feature branch per issue; commit per task per the project's git
discipline. Merge when CI green.

### 7. Cut a matching version

When all non-deferred items for a Python release are ported, cut a
`forge-sdk` release with the matching version number:

```bash
# Bump workspace.package.version in Cargo.toml
cargo build --workspace
git add Cargo.toml Cargo.lock && git commit -m "chore: bump workspace version to 0.1.X"
git tag -a v0.1.X -m "v0.1.X: parity with claude-agent-sdk Python v0.1.X"
# Push main + tag (gated — see project CLAUDE.md override).
```

### 8. Update `PARITY.md`

Prepend a new entry under `## Parity log` using the template at the top
of `PARITY.md`. Include:

- Upstream range reviewed (`<PREV>..<NEW>`).
- Upstream commit SHAs.
- Change classification counts.
- Commits / PRs on forge.
- Deferred items with rationale.
- forge-sdk tag released.
- Notes for next run.

### 9. Metrics worth tracking

- **Coverage %** — `<mirrored passing>` / `<mirrored total>`. Target: 100%.
- **Mirror-ratio** — `<mirrored>` / `<total Python tests in upstream>`.
  Target: 100% modulo explicit skips.
- **Skip count with rationales** — grep skip-stubs in
  `tests/python_parity/`, count.

## When something doesn't mirror cleanly

- **Python-interpreter-specific behaviour** (pickling, asyncio
  semantics, metaclass tricks) — skip with a stub + rationale comment.
- **Changed wire shape where upstream is fixing a bug** — port the fix
  and prefer the new shape; note in the parity log.
- **Ambiguous semantics** — write a small Python script that exercises
  the behaviour against the real `claude` binary and capture the
  observed output; mirror that. Update `docs/protocol-notes.md` if the
  observation changes our understanding.

## Revocation

If upstream decides a released feature was a mistake and removes it in
a subsequent release, our corresponding Rust code and tests should be
removed in the same week. Skipping a deprecated feature silently leads
to ghost parity — avoid.
