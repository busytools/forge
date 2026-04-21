# Python SDK parity tracking

This file is the single source of truth for **where forge-sdk is** relative to Python `claude-agent-sdk` upstream. Every parity-check run (weekly — see below) updates this file.

## Current state

| Field | Value |
|---|---|
| **Target Python SDK version** | `0.1.64` (released 2026-04-20) |
| **Target Python SDK commit** | `anthropics-claude-agent-sdk-python-1267352` (tarball SHA prefix) |
| **forge-sdk version at parity** | `v0.1.3` |
| **Last full parity run** | 2026-04-21 (first run, pulled ahead of the 2026-04-27 weekly) |
| **Next parity check due** | 2026-04-27 (first weekly) |
| **Design-spec basis** | `~/.claude-subspace/plans/2026-04-21-forge-sdk-port-design.md` + `~/.claude-subspace/plans/2026-04-21-forge-sdk-m0-m1-plan.md` + `~/.claude-subspace/plans/2026-04-21-forge-sdk-m2-m3-plan.md` + `~/.claude-subspace/plans/2026-04-21-forge-sdk-m4-m7-plan.md` + `~/.claude-subspace/plans/2026-04-21-forge-sdk-corrections.md`. |

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
- **Ported in forge-sdk:** <commit SHAs on vedhavyas/forge or link to PR>
- **Deferred:** <list + reason>
- **forge-sdk tag released:** vX.Y.Z (mirrors Python version)
- **Notes:** <anything the next parity run should remember>

-->

### 2026-04-21 — Python SDK v0.1.64 (first run, pulled ahead of weekly cadence)

- **Upstream range reviewed:** initial parity — no prior run.
- **Upstream target:** v0.1.64 (latest; no newer releases to port).
- **Changes classified** (against what forge-sdk shipped in v0.1.2):
  - `behavioural`:
    - `transcript_mirror` frame wire shape — Python emits at **top level**
      `{"type":"transcript_mirror","filePath":"...","entries":[...]}`
      (`_internal/query.py:282-289`,
      `_internal/transcript_mirror_batcher.py:3`). forge-sdk v0.1.2 had
      the wrong best-guess shape (nested under system with
      `session_id`/`project_key`/`subpath` inline). FIXED in v0.1.3.
    - `SessionStoreListEntry` field name — Python is `mtime` (not
      `mtime_ms`) per `types.py:1153-1159`. FIXED.
    - `project_key` sanitisation — Python is `[^a-zA-Z0-9]` → `-` with
      djb2 hash suffix for >200 chars (`_internal/sessions.py::_sanitize_path`).
      forge-sdk v0.1.2 used `[^a-zA-Z0-9_-]` → `_` (no hash fallback).
      FIXED.
    - `file_path_to_session_key` resolution — Python derives the
      `SessionKey` from `filePath` relative to projects_dir
      (`_internal/session_store.py:108-153`). ADDED in v0.1.3 as
      `session_store::file_path_to_session_key()`; Client resolves via
      the new `Options.projects_dir` field (falls back to
      `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`).
  - `new-public-api`:
    - `Options::projects_dir` — new field + `OptionsBuilder::projects_dir()`.
  - `trivial`:
    - Docs / comments updated.
- **Ported in forge-sdk:**
  - `8ec1d70` fix(forge-sdk): correct transcript_mirror wire shape +
    mtime field name + project_key sanitisation.
- **Deferred (not regressions, flagged for follow-up):**
  - Full `TranscriptMirrorBatcher` — Python SDK coalesces by
    `file_path`, eager-flushes at 500 entries or 1 MiB, explicit flush
    on `result` arrival with 60 s per-append timeout, `on_error`
    callback surfaces failures
    (`_internal/transcript_mirror_batcher.py:39-174`). forge-sdk v0.1.3
    has a minimum-viable handler: per-frame append, flush-on-result
    as a no-op hook (ready for mechanical swap). Tracked for a future
    patch — moderate effort, low parity risk since at-most-once
    semantics are preserved.
  - Session helper functions (`list_sessions`, `get_session_info`,
    `get_session_messages`, `list_subagents`, `get_subagent_messages`,
    `project_key_for_directory`, and their `_from_store` async
    counterparts from `_internal/sessions.py`) — not exposed in
    forge-sdk. These are read-side utilities that scan
    `~/.claude/projects/<project_key>/*.jsonl` transcripts and do
    JSONL head/tail parsing. Out of scope for v0.1.x — the `Client`
    live-session surface is complete; offline transcript utilities
    can land incrementally.
  - `control_cancel_request` — Python handles an inbound
    `{"type":"control_cancel_request","request_id":...}` by cancelling
    the in-flight control handler (`_internal/query.py:274-280`).
    forge-sdk's classifier-style loop doesn't currently respect this;
    not currently hit in integration paths. Low priority.
- **forge-sdk tag released:** `v0.1.3` (no Python version bump — still
  tracking `v0.1.64`).
- **Upstream `tests/` mirror status:** no Rust port of `tests/` yet.
  Counted ~30 Python test files against the published behaviour. The
  test-mirroring subdirectory `crates/forge-sdk/tests/python_parity/`
  is empty; mirroring begins next weekly run as the contract.
- **Notes for next parity run (2026-04-27):**
  - Verify `transcript_mirror` wire shape + `file_path_to_session_key`
    resolution against the real `claude` CLI (manual observation with
    `--session-mirror`); correct any drift.
  - Start Python-tests → `tests/python_parity/` mirror.
  - Re-check upstream for new releases since v0.1.64.
  - Consider implementing the full batcher with size thresholds (500 /
    1 MiB).

## How to run a parity check

Follow `docs/parity-check.md` (lands in Plan 3 Task 25). In short:

1. `gh release list --repo anthropics/claude-agent-sdk-python --limit 10`
2. Identify new releases since the last logged parity run.
3. For each new release, `gh api repos/anthropics/claude-agent-sdk-python/compare/<prev>...<new>`
4. **Diff `tests/` separately.** Upstream's test suite is a behavioural spec. Every new or changed test there should translate into a corresponding Rust test in `crates/forge-sdk/tests/python_parity/` (see next section).
5. Classify each source diff hunk (trivial / behavioural / new-public-api) AND each test diff (new coverage / modified expectation / removed test).
6. Open one GitHub issue per non-trivial item on `vedhavyas/forge`.
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
