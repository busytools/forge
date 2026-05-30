# Cut - tracing_bridge module

**Cut on:** 2026-04-23
**Commit:** *(this commit)*
**Branch:** `cut/tracing-bridge`
**Parity impact:** forge-sdk diverges from Python `claude-agent-sdk` v0.1.62+ on this surface. Python ships an equivalent helper module; forge-sdk used to mirror it.

## What the module did

45 LoC exporting three `tracing` span helpers for downstream consumers:

```rust
pub fn turn_span(session_id: &str) -> Span
pub fn tool_span(tool_name: &str, tool_use_id: &str) -> Span
pub fn hook_span(kind: &str, callback_id: &str) -> Span
```

Each wraps `tracing::info_span!` with a conventional span name
(`forge_sdk.turn` / `forge_sdk.tool` / `forge_sdk.hook`) and a fixed
set of structured fields. Callers used `span.enter()` for RAII-scoped
context.

## Why we cut it

- **Zero internal users.** forge-sdk itself never enters these spans.
  The module was purely a downstream-consumer convenience.
- **Zero tests.** No coverage, no regression surface.
- **Thin value.** Each helper saves ~15 characters over writing the
  equivalent `info_span!(…)` inline at the call site. The field names
  aren't enforced anywhere - they're a convention, not a protocol.
- **Parity inheritance.** Module existed to mirror Python SDK's
  v0.1.62+ tracing bridge. Post-parity phase, parity-for-its-own-sake
  isn't a keep-reason.
- If forge-sdk later instruments its own internals (span per
  `next_event`, per tool dispatch, per hook callback), the spans
  belong inline where they're entered - not in a separate re-usable
  module that nobody calls.

## What was removed

- `crates/forge-sdk/src/tracing_bridge.rs` (entire file)
- `pub mod tracing_bridge;` line in `crates/forge-sdk/src/lib.rs`

No other files affected - the module had no dependents.

## How to bring it back

When forge-sdk (or a downstream consumer) concretely needs
standardised span helpers:

1. **Restore the source file from git.** `git show <pre-cut-commit>:crates/forge-sdk/src/tracing_bridge.rs > crates/forge-sdk/src/tracing_bridge.rs`
2. **Re-add the module declaration** to `lib.rs`:
   ```rust
   pub mod tracing_bridge;
   ```
3. **Python SDK reference** (if spans need to match Python field
   names): search the upstream `src/claude_agent_sdk/` tree for
   `turn_span`, `tool_span`, `hook_span` - they were added in Python
   SDK v0.1.62 and remain in v0.1.64.

## Alternative: do nothing, instrument inline

If forge-sdk later wants internal spans around turn/tool/hook
processing, prefer introducing them **at the call site** in
`client.rs::next_event` (and friends) rather than reintroducing a
standalone helper module. The call sites already have the fields
(`session_id`, `tool_name`, etc.) in scope - a helper module just
adds an extra hop.
