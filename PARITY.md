# Python SDK parity tracking

This file is the single source of truth for **where forge-sdk is** relative to Python `claude-agent-sdk` upstream. Every parity-check run (weekly — see below) updates this file.

## Current state

| Field | Value |
|---|---|
| **Target Python SDK version** | `0.1.64` (released 2026-04-20) |
| **Target Python SDK commit** | TBD — record SHA on first parity run |
| **forge-sdk version at parity** | `pre-v0.0.1` — nothing shipped yet |
| **Last full parity run** | _not yet executed_ |
| **Next parity check due** | 2026-04-27 (first Monday after planning) |
| **Design-spec basis** | `~/.claude-stargate/plans/2026-04-21-forge-sdk-port-design.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-m0-m1-plan.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-m2-m3-plan.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-m4-m7-plan.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-corrections.md` (if present — apply before executing Plans 2/3). |

## Parity log

Each entry below records one weekly parity check.

<!-- New entries prepended here. Template:

### <YYYY-MM-DD> — Python SDK vX.Y.Z

- **Upstream range reviewed:** `<previous-tag>..<new-tag>`
- **Upstream commit SHAs:** `<sha1>`, `<sha2>`, ...
- **Changes classified:**
  - `trivial`: <list>
  - `behavioural`: <list>
  - `new-public-api`: <list>
- **Ported in forge-sdk:** <commit SHAs on busytools/forge or link to PR>
- **Deferred:** <list + reason>
- **forge-sdk tag released:** vX.Y.Z (mirrors Python version)
- **Notes:** <anything the next parity run should remember>

-->

_(No parity runs yet.)_

## How to run a parity check

Follow `docs/parity-check.md` (lands in Plan 3 Task 25). In short:

1. `gh release list --repo anthropics/claude-agent-sdk-python --limit 10`
2. Identify new releases since the last logged parity run.
3. For each new release, `gh api repos/anthropics/claude-agent-sdk-python/compare/<prev>...<new>`
4. **Diff `tests/` separately.** Upstream's test suite is a behavioural spec. Every new or changed test there should translate into a corresponding Rust test in `crates/forge-sdk/tests/python_parity/` (see next section).
5. Classify each source diff hunk (trivial / behavioural / new-public-api) AND each test diff (new coverage / modified expectation / removed test).
6. Open one GitHub issue per non-trivial item on `busytools/forge`.
7. Port during the week; cut a `forge-sdk` release with the matching version number.
8. **Update this file** with a new parity-log entry, including:
   - Count of Python tests mirrored into the Rust suite this week.
   - Any Python tests intentionally skipped, with rationale.

## Test-mirroring strategy (core of parity proof)

Python SDK's `tests/` directory *is* the executable spec for behavioural parity. Strategy:

1. **Maintain a `crates/forge-sdk/tests/python_parity/` subdirectory.** One Rust test file per Python test file (e.g. `tests/test_client.py` → `tests/python_parity/client.rs`). Each mirrored Rust test keeps the original Python test name as a comment header + translates the body to forge-sdk's API.
2. **Port, don't translate mechanically.** Python test fixtures (pytest, `unittest.mock.AsyncMock`) have no 1:1 Rust equivalent. Port the *behavioural assertion* — "calling X with Y produces Z" — using Rust idioms (`nextest`, tokio::test, mock fixtures we already have).
3. **Tag each mirrored test with the upstream commit/version it was ported from:**
   ```rust
   /// Ported from claude-agent-sdk-python v0.1.64 tests/test_client.py::test_query_emits_init
   #[tokio::test]
   async fn query_emits_init() { ... }
   ```
   This makes weekly diffs trivial (`grep v0.1.64 tests/python_parity/`).
4. **Skip tests that don't make sense for Rust.** Python-specific behaviour (pickling, asyncio-specific semantics, Python-interpreter edge cases) is out of scope. When skipping, leave a commented stub with the skip rationale — never silently omit.
5. **The mirrored suite is *additive*, not exclusive.** We also keep the Rust-specific tests (mocks, type roundtrips, etc.) from Plans 1–3. Mirrored tests catch behavioural regressions against upstream; native Rust tests catch our implementation bugs. Both needed.

### Why this works

- Upstream tests encode every behavioural contract the SDK exposes. Mirroring them forces feature-level parity to be *testable*, not just claimed.
- When Python ships a new test, mirroring it in Rust the same week is a concrete, completable parity task — no ambiguity about "did we cover this?"
- Regressions in forge-sdk that accidentally break parity fail the python_parity suite before anyone notices.

### Metrics worth tracking in the parity log

- **Coverage %:** <mirrored tests passing> / <mirrored tests total>. Target: 100%.
- **Mirror-ratio:** <mirrored tests> / <total Python tests in current upstream version>. Target: 100% modulo explicitly-skipped tests.
- **Skip count with rationales:** grep the skip-stubs, count them, list reasons.

## Why this matters

Anthropic ships `claude-agent-sdk-python` releases on a ~3–4/month cadence. If forge-sdk falls a month behind, we've re-created the problem we built forge-sdk to solve (rusty community crates lagging upstream). The weekly cadence is the forcing function.

## Reminder to the user

The forge lead MUST proactively remind the user every Monday (or the start of the working week) that a parity check is due. The Claude Code harness does not automate this; the lead's job includes surfacing it.
