# Upstream watch (Python claude-agent-sdk idea scan)

forge does **not** feature-parity-track Python's `claude-agent-sdk`.
Both projects wrap the same `claude` CLI and share a wire contract with
that binary, nothing more. This is an idea-scanning exercise, not a
port-everything ritual.

## When to invoke

- Monday, or the first working day of the week, as a proactive nudge:

  > "Upstream-watch Monday. Python `claude-agent-sdk` last reviewed at
  > `<version>`. Want me to scan for new features worth pulling in?"

- Any time the user asks what upstream has been doing.
- Do not run it unprompted mid-week; it is a low-urgency scan.

## The scan

1. Diff Python `src/` and `tests/` against the previously-reviewed
   version, or against the recorded baseline in
   `~/.claude-subspace/plans/upstream-watch-<date>.md`.
2. For each new public API, hook event, control_request subtype, or
   stream-json variant, ask: does this make forge more capable **for our
   use case**? If yes, propose a port, but port it the forge-native way
   rather than mirroring Python's API shape.
3. New stream-json shapes the CLI actually emits are a different
   category: those are wire facts, not parity choices, and MUST be
   supported in the decoder. Surface them to the user and land the
   decoder change plus a wire-conformance scenario in the same week (see
   Hard Rule #9 and the `claude-cli-upgrade` skill).
4. Record the reviewed version so the next scan has a baseline.

## What this is not

There is no contract mapping every public Python type to a Rust one, and
no test-mirroring requirement. The old `PARITY.md` lineage is archived.
Drop `crates/forge-sdk/tests/python_parity/` 1:1 mappings whenever they
get in the way of a cleaner Rust API; keep only tests that cover
behaviour we actually care about.

## Related

- `.claude/skills/claude-cli-upgrade/` - the mandatory path when the
  `claude` binary itself moves, which is a wire-conformance matter
  rather than an idea scan.
