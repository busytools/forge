# PARITY.md — archived

This file used to track strict feature-parity between forge-sdk and
Python's `claude-agent-sdk`. **That contract was retired on
2026-04-27** when the project pivoted to a "peer reference
implementation" model. forge is no longer a port of Python's SDK; it
is its own Rust-native client of the `claude` CLI, free to be simpler
and more capable where the language permits.

The full parity log up to v0.1.64 is preserved in git history (search
for commits touching this file before the 2026-04-27 retirement
commit). It served its purpose getting forge-sdk to behavioural
completeness against the CLI; it's not load-bearing going forward.

## What replaced it

See `CLAUDE.md` → "Weekly upstream-watch" — the Monday ritual is now
about *scanning Python upstream for new features worth pulling in*,
not enforcing a per-symbol mapping. New scans are recorded in
`~/.claude-stargate/plans/upstream-watch-<YYYY-MM-DD>.md` (user
plans, not tracked in this repo).

The wire-conformance harness (`crates/forge-test-harness/`) remains
the only hard contract — between forge-sdk and the `claude` CLI's
stream-json, and between forge-daemon and its WebSocket clients.
That's behavioural correctness against external systems, not
parity-with-Python.

## What got dropped

- The "every Python public type has a Rust counterpart" rule.
- The `crates/forge-sdk/tests/python_parity/` 1:1 test-file mapping
  (the directory may stay during the transition; new SDK work isn't
  required to maintain the mirror).
- The `docs/forge-sdk-parity-map.html` parity-coverage view (file
  may stay as a current-surface map but the parity-coverage column
  is moot).
- `docs/parity-check.md` runbook — superseded by the upstream-watch
  shape in `CLAUDE.md`.
