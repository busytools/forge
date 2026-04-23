# Cut — `ToolPermissionContext::signal` field + `with_signal` builder

**Cut on:** 2026-04-24
**Commit:** *(this commit)*
**Branch:** `cut/worktree-support` (rolling post-parity cleanup batch)
**Parity impact:** forge-sdk diverges from Python `claude-agent-sdk` v0.1.64 on this surface. Python reserves `ToolPermissionContext.signal` for future abort-signal wiring (their upstream hasn't shipped the protocol either); forge-sdk previously mirrored the field as a placeholder.

## What the field was

```rust
// ToolPermissionContext
pub signal: Option<Value>,

// Builder
#[doc(hidden)]
pub fn with_signal(mut self, signal: Value) -> Self { … }
```

A placeholder `Option<Value>` on the context the SDK hands to the `can_use_tool` callback. In Python SDK v0.1.64 the equivalent field (`types.py:178`) is reserved for a future **abort signal** — an async cancellation primitive (analogous to JavaScript's `AbortController` or Python's `asyncio.CancelledError`) that would let the CLI tell the callback "the user wants to cancel this check, give up early." The wire delivery + callback-side consumption aren't specified by Anthropic yet.

## Why we cut it

- **Zero users.** The field has never held a real value. `control_dispatch.rs` constructs `ToolPermissionContext` without ever setting `signal`; the `with_signal` builder is `#[doc(hidden)]` and never called anywhere in the tree.
- **Pure parity placeholder.** We mirrored Python because Python had the field, not because we use it.
- **Upstream protocol undefined.** If we kept the field "for the future," we'd still need to rewrite its shape when the spec lands — `Option<Value>` is a weak guess at the eventual type.
- **`non_exhaustive` protects us.** `ToolPermissionContext` is already `#[non_exhaustive]`; adding fields later isn't a breaking change for callers constructing via `::new(...)`. No API-stability reason to pre-reserve.

## What was removed

- `ToolPermissionContext::signal: Option<Value>` field (`src/permissions.rs`).
- `ToolPermissionContext::with_signal(…)` builder method + its `#[doc(hidden)]` marker.
- The `signal: None` initialiser line in `ToolPermissionContext::new(…)`.
- Two docstring references to `signal` in `new()`'s rustdoc.

## How to bring it back

When Anthropic's Python SDK (or the CLI directly) ships the abort-signal protocol:

1. **Check the actual wire shape.** Python's type was `Any | None`; the real protocol may land as a different shape (e.g., a cancellation handle correlating to a subsequent `control_cancel_request`). Don't reintroduce as `Option<Value>` just because that's what was here — model the correct type.
2. **Restore the field** to `ToolPermissionContext`. Because the struct is `#[non_exhaustive]`, adding a new field isn't breaking for downstream callers that construct via `::new(...)`.
3. **Wire the dispatch path.** In `client/control_dispatch.rs::handle_control`, thread the abort-signal value from the incoming `ControlRequestKind::CanUseTool` variant into `ToolPermissionContext::new(...).with_signal(...)`. This requires adding a `signal` field to `ControlRequestKind::CanUseTool` in `src/control.rs` first.
4. **Update bring-back in `docs/cuts/transcript-mirror.md` style** once restored — the callback integration story (how a callback observes cancellation mid-await) is the real design question.

Reference implementation for the wire shape, once available: Python SDK's `_internal/query.py` cancellation handling.

## Non-impact

- No API break for existing callers — the struct is `non_exhaustive`, and `::new(...)` callers never touched the field.
- No behavioural change — nothing read the field; removing it removes zero code paths.
