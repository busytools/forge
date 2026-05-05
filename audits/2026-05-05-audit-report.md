# Audit Report — 2026-05-05

## Scope
Full codebase audit. 244 .rs files across 5 crates (forge-primitives, forge-sdk, forge-agent, forge-tui, forge-test-harness). Working tree clean at squash commit `77860ad` (forge-tui v0.13.0, post-restructure).

## Specialists Deployed
- dead-code-analyzer
- architecture-reviewer
- bug-hunter
- silent-failure-hunter
- code-simplifier

## Start Here

1. **[Critical] crates/forge-agent/src/forge_sdk_bridge.rs:115-220** — `client.lock().ok()` discards Mutex poisoning across the entire bridge surface. A panic anywhere in SDK actor code poisons every lock and the bridge silently goes dark — every `dispatch()` returns "before active session", `set_client()` becomes a no-op, and `Drop` silently fails to abort outstanding git watchers. Fix: switch to `parking_lot::Mutex` (already a workspace dep, never poisons).

2. **[Critical] crates/forge-agent/src/forge_sdk_worker.rs:454-456, 493-495** — Permission/question parking-lot `Mutex` poisoning silently drops registration. `if let Ok(mut map) = pending.lock()` collapses poisoned-lock and downstream-failure paths into the same generic "response channel closed" deny. Every subsequent permission/question request quietly denies with no log.

3. **[Important] crates/forge-tui/src/app/permissions.rs:152-154** — ESC on a permission prompt picks the **last option** as a fake "cancel". For default options that lands on `deny`, but for custom option sets (e.g. only Allow Once + Allow Always) ESC sends Allow Always — silently approving when the user meant to cancel. Fix: send `RequestPermissionOutcome::Cancelled` (the test-only `respond_permission_cancel` shows the shape).

4. **[Important] crates/forge-tui/src/app/events/rate_limit.rs:26** — `format_resets_at` calls `Duration::from_secs_f64(epoch_secs)` with no guard. Negative/NaN/infinite values panic the TUI process and lose unsaved input. Sibling functions correctly guard. Fix: early-return on `!is_finite() || < 0.0`.

5. **[Important] forge-tui parallel type universe** — `forge-tui/src/agent/model.rs` (1256 lines) + `app/connect/type_converters.rs` (1019 lines, ~640 of mapping) re-define ~30 wire types that are already present in `forge_primitives` and write 13 `map_*` shims to translate between them. Architecture-reviewer and code-simplifier converged here independently. Every new wire-shape addition costs three edits instead of one.

## Summary
2 critical · 13 important · 16 minor · 9 possible

---

## Critical Findings

### crates/forge-agent/src/forge_sdk_bridge.rs:115-220
**`client.lock().ok()` discards Mutex poisoning across bridge surface**
- **Found by:** silent-failure-hunter (high)
- **Description:** Every bridge accessor wraps `self.inner.client.lock()` with `.ok()` (lines 115-127, 119-123, 159-204, 213-220). A `Mutex` panic anywhere in SDK actor code poisons every lock for the lifetime of the process.
- **Impact:** After poisoning, `client()` returns `None`, every `dispatch()` returns "before active session", `set_client()` silently becomes a no-op, and `Drop` silently fails to abort outstanding git watchers. UI shows "agent disconnected" with no reason. Git watcher tasks leak.
- **Suggested fix:** Convert the slot to `parking_lot::Mutex` (already a workspace dep, never poisons). Mechanical rewrite.

### crates/forge-agent/src/forge_sdk_worker.rs:454-456, 493-495
**Permission/question pending-map `Mutex` poisoning silently drops registrations**
- **Found by:** silent-failure-hunter (high)
- **Description:** `if let Ok(mut map) = pending.lock()` silently drops registration when the `Mutex` is poisoned. Distinguishable bug paths (poisoned lock vs. genuine downstream failure) collapse to the same generic "response channel closed" deny. Same pattern in `take_pending` (line 612) and `deliver_question_response` (line 597).
- **Impact:** When a `Mutex` panic poisons the lock, every subsequent permission and question request quietly denies with no log. User sees mysterious blanket-deny behaviour.
- **Suggested fix:** Either log `tracing::error!` on the `Err` branch naming the poisoned lock and `tool_use_id`, or (cleaner) switch these maps to `parking_lot::Mutex` alongside the bridge slot in the previous finding.

---

## Important Findings

### Cargo.toml + crate manifests
**Unused workspace + crate-local dependencies**
- **Found by:** dead-code-analyzer (high)
- **Description:** `futures-util = "0.3"` declared in workspace `Cargo.toml:31` but no crate consumes (forge-tui uses `futures` directly). Crate-local unused deps: `forge-sdk: uuid, unicode-normalization`; `forge-tui: async-trait, reqwest, tokio-util, which`; `forge-test-harness: serde, tempfile (dev)`; `forge-primitives: thiserror`.
- **Impact:** Cargo bloat, slower clean builds, drift between declared and real dep graph.
- **Suggested fix:** Drop each unused dep, verify with `cargo build --all-targets`. Long-term, run `cargo +nightly udeps` in CI.

### crates/forge-agent/src/agent.rs:107-108, 119-121
**`serde_json::to_value(launch_settings).unwrap_or(Null)` swallows session-spawn config corruption**
- **Found by:** silent-failure-hunter (high)
- **Description:** `Agent::new_session` and `resume_session` pack `launch_settings` via `serde_json::to_value(...).unwrap_or(Value::Null)`. On serialise failure the launch payload becomes `Null` and dispatch falls through to default settings.
- **Impact:** User configures model/permission mode in `settings.json`, hits a serialise edge case, new session silently launches with default model + default `Ask` permission mode. No log.
- **Suggested fix:** `.map_err(|e| anyhow::anyhow!("..."))?` to propagate, or at minimum `tracing::error!` on the `Err` branch.

### crates/forge-agent/src/cloud/oauth_credentials.rs:69-141
**Disk + keychain reader collapses every error to `None`**
- **Found by:** silent-failure-hunter (high)
- **Description:** `load_oauth_credentials_at` and `load_oauth_credentials_from_keychain` chain `.ok()?` across `read_to_string`, `from_str`, `from_utf8`, `parse`. Every distinguishable failure (file missing, unreadable, malformed JSON, base64 corruption) collapses to `None`.
- **Impact:** User with corrupt `credentials.json` sees the same `None` as a brand-new install — login UX has no way to surface the actual problem.
- **Suggested fix:** Add `tracing::debug!` on each `.ok()?` chain, or restructure to `Result<Option<...>, Error>`.

### crates/forge-agent/src/cloud/service_status.rs:58-76
**Network/parse errors silently treated as "service healthy"**
- **Found by:** silent-failure-hunter (high)
- **Description:** `fetch_service_status` returns `None` for client build failure, HTTP send failure, JSON deserialise failure, AND "all healthy". The four are indistinguishable.
- **Impact:** UI silently shows no status banner during an actual incident if statuspage itself is down or returns malformed JSON.
- **Suggested fix:** Distinguish via `Result<Option<ServiceIssue>, FetchError>` so the caller can surface "couldn't reach statuspage" separately from "no incident".

### crates/forge-agent/src/client.rs:32-192 (trait) + crates/forge-agent/src/forge_sdk_bridge.rs:224 (sole impl)
**`AgentBridge` trait has one impl, dispatched against concrete type anyway**
- **Found by:** code-simplifier (high)
- **Description:** Trait defines 35+ async methods; only one implementor (`ForgeSdkBridge`). Dispatcher calls through `&ForgeSdkBridge`, never `&dyn AgentBridge`. Trait is `pub(crate)` — no external impl is possible. Test-stub path constructs a real `ForgeSdkBridge`. ~200 lines of dead abstraction.
- **Impact:** Mental overhead: every method has two definitions to keep in sync.
- **Suggested fix:** Remove the trait. Convert each method to inherent on `ForgeSdkBridge`. Update `crate::client` re-exports.

### crates/forge-agent/src/forge_sdk_bridge.rs:318, 332, 346, 359, 362, 376, 409, 431, 453, 482, 493, 511, 536, 559, 578
**Terminal `AgentEvent` sends silently dropped on closed channel**
- **Found by:** silent-failure-hunter (medium)
- **Description:** Every `let _ = event_tx.send(AgentEvent::...)` silently drops events when the receiver is gone. Most cases are benign shutdown signal — but for terminal/error variants (`RuntimeReloadFailed`, `McpOperationError`, `ConnectionFailed`) the App is the only consumer that surfaces failures. A racing tear-down will silently swallow user-visible failure events.
- **Suggested fix:** For terminal/error variants only, wrap with `if event_tx.send(...).is_err() { tracing::warn!(...); }`.

### crates/forge-agent/src/tooling.rs:340, 654
**`serde_json::from_str(trimmed).ok()` swallows malformed structured-output JSON**
- **Found by:** silent-failure-hunter (medium)
- **Description:** A tool that returns a structured payload but malformed JSON (truncated streaming, escaping bug) silently degrades to plain-text rendering with no debug breadcrumb.
- **Suggested fix:** `.inspect_err(|e| tracing::debug!(...))` on the parse so debugging is possible.

### crates/forge-agent/src/userdata/catalog/mutations.rs:61, 94, 133
**Catalog mutation helpers unused — `Command::RewindFiles` actively no-ops**
- **Found by:** dead-code-analyzer (high)
- **Description:** `tag_session`, `delete_session`, `fork_session` + `ForkSessionResult` unused outside intra-module tests. `Command::RewindFiles` logs "not yet wired; dropping". ~250 lines of half-wired feature.
- **Suggested fix:** Decide: (a) wire up `Command::RewindFiles` + session-picker delete/tag/fork flows, or (b) delete and revisit. The current state is the worst of both — code compiles but the feature silently does nothing.

### crates/forge-agent/src/userdata/catalog/scan.rs:79, 103, 352
**Catalog scan helpers (read-side counterparts to mutations) unused**
- **Found by:** dead-code-analyzer (high)
- **Description:** `list_subagents`, `get_subagent_messages`, `get_session_info` have no production callers. Read-side counterparts to the F9 mutations.
- **Suggested fix:** Same disposition as the mutations finding above.

### crates/forge-agent/src/userdata/catalog/scan.rs:415-446
**Lite-read function chains 6+ `.ok()?` calls, masking session-scan failures**
- **Found by:** silent-failure-hunter (medium)
- **Description:** Every `.ok()?` (open, metadata, read, seek, read) collapses to `None` → `list_sessions` silently drops the session. A user with one or two sessions whose files have permission errors sees them missing from the picker with no indication.
- **Suggested fix:** `tracing::warn!` at function entry plus a step name on each failure branch.

### crates/forge-primitives/src/session_meta.rs:11-67
**Five orphan ACP-protocol-parity types**
- **Found by:** dead-code-analyzer (high)
- **Description:** `AuthMethod`, `AgentCapabilities`, `InitializeResult`, `SessionInit`, `McpSetServersResult` are completely unreferenced. `SessionListEntry` and `PromptChunk` from the same module *are* used.
- **Suggested fix:** Delete the five types and the corresponding `lib.rs` `pub use` line.

### crates/forge-primitives/src/session_update.rs:169-179 + crates/forge-tui/src/app/connect/type_converters.rs:210-232 + crates/forge-tui/src/app/events/sdk_message.rs:931-957
**`RateLimitUpdate` wire variant inlines fields, forcing destructure-rebuild**
- **Found by:** code-simplifier (high)
- **Description:** A wire-shape `RateLimitUpdate` exists at `forge_primitives::RateLimitUpdate` but `SessionUpdate::RateLimitUpdate` inlines 9 fields rather than wrapping `RateLimitUpdate(RateLimitUpdate)`. Two near-identical 18-line repack blocks. Same shape for `ApiRetryUpdate` (5 fields) and `SettingsParseError` (3 fields).
- **Suggested fix:** Replace inlined variants with single-field wrappers (`#[serde(transparent)]` or `flatten` to preserve wire compat). Drop the repack blocks.

### crates/forge-tui/src/agent/model.rs (1256 lines) + crates/forge-tui/src/app/connect/type_converters.rs (1019 lines)
**TUI maintains a parallel type universe mirroring `forge_primitives`**
- **Found by:** architecture-reviewer (high), code-simplifier (high)
- **Description:** `model.rs` defines ~30 types byte-equivalent to wire shapes already present in `forge_primitives`; `type_converters.rs` writes 13 `map_*` shims to translate between them. Verified byte-equivalent: `RateLimitStatus`, `FastModeState`, `ApiRetryError`, `RuntimeSessionState`, `ToolCallStatus`, `Diff`, `McpResource`, `ToolCallLocation`, `PermissionOption{,Kind}`, `QuestionOption`, `RateLimitUpdate`, `CompactionTrigger`, `SessionStatus`, `EffortLevel`. ~20 forge-tui call sites import `forge_primitives` directly while ~120 use `crate::agent::model` — two universes in flight.
- **Impact:** Every new wire-shape addition has to be triple-handled (define in primitives, redefine in `model.rs`, write a `map_*` shim). 642 lines of pure ceremony adding zero behaviour.
- **Suggested fix:** Migrate forge-tui to consume `forge_primitives::*` directly. Stage in batches; pick the next 5 byte-equivalent pairs (`RateLimitStatus`, `FastModeState`, `ApiRetryError`, `RuntimeSessionState`, `ToolCallStatus`), drop the model versions and the corresponding `map_*` fns. Re-frame as the natural endpoint of issue #38.

### crates/forge-tui/src/agent/model.rs:893-925, 211-219
**Wire ↔ Model parallel enums (~7) with 1:1 variant mappings**
- **Found by:** code-simplifier (high)
- **Description:** `FastModeState`, `RateLimitStatus`, `ApiRetryError`, `RuntimeSessionState`, `SessionStatus`, `CompactionTrigger`, `EffortLevel` — duplicated wire side ↔ model side with identical shape. `EffortLevel` mapping duplicated 3 times.
- **Suggested fix:** Re-export from `forge_primitives`. Delete the `map_*` fns. Strict subset of the parallel type universe finding above; this is the lowest-risk first batch.

### crates/forge-tui/src/app/connect/type_converters.rs:389-540
**`convert_tool_call`, `convert_tool_call_to_fields`, `convert_tool_call_update_fields` triplicate**
- **Found by:** code-simplifier (high)
- **Description:** Three nearly-identical conversion functions, ~150 LoC of duplication. `convert_tool_call_to_fields` is a chimera taking `T` but building `Option<T>`-shaped output.
- **Suggested fix:** Extract a `convert_locations` helper plus `From<ToolCall> for ToolCallUpdateFields`. Eliminates one of the three.

### crates/forge-tui/src/app/events/rate_limit.rs:26
**`Duration::from_secs_f64` panic on negative epoch**
- **Found by:** bug-hunter (high)
- **Description:** `format_resets_at(epoch_secs)` calls `Duration::from_secs_f64(epoch_secs)` with no guard. Rust panics on negative/NaN/infinite. Sibling functions in the same file correctly guard. Wire's `RateLimitInfo.resetsAt` is `i64`; `number_field` only filters `is_finite()`.
- **Impact:** Negative `resetsAt` (clock skew, CLI bug) crashes the TUI process. Loss of unsaved input.
- **Suggested fix:** `if !epoch_secs.is_finite() || epoch_secs < 0.0 { return "now".to_owned(); }` matches sibling style.

### crates/forge-tui/src/app/events/sdk_message.rs:63
**`serde_json::to_value(&msg).unwrap_or(Value::Null)` masks SDK envelope corruption**
- **Found by:** silent-failure-hunter (high)
- **Description:** Dispatcher serialises every received SDK `Message` back to JSON to read fields. On serialise failure the entire raw envelope becomes `Null`. Every per-variant handler reading raw silently sees "missing fields" and skips.
- **Impact:** Wire-level corruption silently disables fast-mode tracking, error capture, and terminal-reason classification.
- **Suggested fix:** Replace with explicit `match`; log on `Err`.

### crates/forge-tui/src/app/events/turn.rs:142-159, 213-234 + permissions.rs:271-317 + questions.rs:295-343
**Permission/question response oneshot send `let _ =` discards send failure**
- **Found by:** silent-failure-hunter (medium)
- **Description:** When App rejects a permission/question request and tries to "reject by selecting last option" via `let _ = response_tx.send(...)`, send failure (receiver dropped) is silent. SDK side then blocks on a never-resolved oneshot until subprocess EOF.
- **Suggested fix:** `tracing::debug!` with `outcome = "no_receiver"` on send failure. Note: the "reject by selecting last option" pattern itself is the bug-hunter ESC finding below — fixing that fixes one of the call sites.

### crates/forge-tui/src/app/events/turn.rs:243 + crates/forge-tui/src/app/input_submit.rs:129 + crates/forge-tui/src/app/state.rs:807-808
**`let _ = app.finalize_in_progress_tool_calls(...)` discards finalisation result on cancel**
- **Found by:** silent-failure-hunter (medium)
- **Description:** A failed finalise leaves orphaned in-progress tool calls — spinners keep ticking after the turn ends.
- **Suggested fix:** Log on `Err`; consider returning the failure instead of swallowing.

### crates/forge-tui/src/app/permissions.rs:152-154
**ESC on permission picks last option (potentially Allow) when no reject option found**
- **Found by:** bug-hunter (high)
- **Description:** ESC fallback `respond_permission(app, Some(option_count - 1))` selects the last option as if the user picked it. For default option ordering the last is `deny` (works), but custom option sets (e.g. only Allow Once + Allow Always) make ESC send Allow Always.
- **Impact:** User intends to cancel, instead approves. Could trigger unintended tool execution.
- **Suggested fix:** Replace with a true cancel: send `RequestPermissionOutcome::Cancelled` (the `#[cfg(test)]` `respond_permission_cancel` shows the shape). This also resolves one of the silent-failure findings above.

### crates/forge-tui/src/app/state.rs:121
**`App` god struct**
- **Found by:** architecture-reviewer (medium)
- **Description:** `App` holds session, render, focus, paste, file-index, plugin, todo, terminal, telemetry, OAuth, retention-policy, cache-budget, and animation state in one struct — ~100 public fields, ~80 public methods, 50 distinct files take `&mut App`. Comments at state.rs:323-324 admit `Option::take()` workarounds for borrow-checker friction. Public-field exposure means any module can mutate any field, defeating invariants.
- **Impact:** Change amplification, testing difficulty, ongoing borrow-checker friction.
- **Suggested fix:** Extend the existing `GitContextState`/`FocusManager`/`PluginsState`/`McpState`/`UsageState` pattern. First targets: `RenderCacheState` and `ToolCallTracker` (combining `tool_call_index` + `tool_call_scopes` invariants).

### crates/forge-agent/src/translate/error_handling.rs:9-17 + crates/forge-tui/src/app/events/sdk_message.rs:1006-1037
**`parse_turn_error_class` round-trips an enum through `&'static str`**
- **Found by:** code-simplifier (high)
- **Description:** `classify_turn_error_kind` returns `&'static str`, immediately passed through `parse_turn_error_class` to convert back to `TurnErrorClass`. No other caller of `parse_turn_error_class`.
- **Suggested fix:** Have the classifier return `TurnErrorClass` directly. Delete `parse_turn_error_class`.

### crates/forge-agent/src/translate/error_handling.rs:163-206 + crates/forge-tui/src/ui/tool_call/errors.rs:73-117 + crates/forge-tui/src/app/events/sdk_message.rs:1039-1046
**Duplicated error-text helpers**
- **Found by:** code-simplifier (high)
- **Description:** Three copies of `extract_xml_tag_value`. Two of `truncate_for_log` / `preview_for_log` (240-char + `\n` → `\\n`). Two of `looks_like_auth_required`. Two pure-passthrough renames in `tool_call/errors.rs:93-95` and `:104-106`.
- **Suggested fix:** Make the originals `pub(crate)`, consume from TUI sites, delete the copies.

---

## Minor Findings

### Cargo.toml:31
**Unused workspace dep `futures-util`** — Found by dead-code-analyzer (high). Drop. forge-tui consumes `futures` directly.

### crates/forge-sdk/src/lib.rs:117-200
**`forge-sdk::query` and `query_stream` have no in-tree callers** — Found by dead-code-analyzer (medium). Top-level helpers, well-documented but no test/example/other-crate use; forge-tui drives via `Client::spawn`. Either add a doctest/example or acknowledge as documented public surface.

### crates/forge-sdk/src/options.rs:449-451
**`OptionsBuilder::cli_path` alias has no callers** — Found by dead-code-analyzer (high). Documented as alias for `binary()`. Delete.

### crates/forge-sdk/src/error.rs:111-116
**`Error::message_parse_with_data` constructor has no callers** — Found by dead-code-analyzer (high). Delete; optionally drop the `data` field on `Error::MessageParse`.

### crates/forge-primitives/src/permissions.rs:83-108
**`ToolPermissionContext::with_suggestions()` and `with_display()` unused** — Found by dead-code-analyzer (high). Either wire the call sites that *should* populate display/suggestions, or drop the builders.

### crates/forge-sdk/src/mcp/tool.rs:33-38
**`ToolOutput::error` constructor unused** — Found by dead-code-analyzer (high). Delete or add example/test.

### crates/forge-sdk/src/mcp/server.rs:39-47
**`McpServer::name()` and `McpServer::tool_names()` getters unused** — Found by dead-code-analyzer (high). Drop unless an in-tree consumer is intended soon.

### crates/forge-agent/src/userdata/memory.rs:33, 40
**`read_project_memory` and `read_claude_md` unused** — Found by dead-code-analyzer (high). Delete unless the TUI is expected to start displaying memory contents.

### crates/forge-sdk/src/control.rs:24-29, 314-319
**`ControlRequestType` / `ControlResponseType` are single-variant enums** — Found by code-simplifier (medium). Each enum has exactly one variant; doc admits "the only current variant." Use `#[serde(rename = ...)]` constants or a typestate-free constant.

### crates/forge-sdk/src/transport.rs:14-47 + crates/forge-sdk/src/transport/process.rs:427-459
**`AsyncWriter` trait has one impl** — Found by code-simplifier (medium). Trait doc admits "one implementor" (`SharedWriter`). Replace `Arc<dyn AsyncWriter>` with `Arc<SharedWriter>` and delete the trait.

### crates/forge-tui/src/app/events/sdk_message.rs:280-285
**`is_bridge_tool_result_block_type` is a one-line pure pass-through** — Found by code-simplifier (high). Inline the call.

### crates/forge-tui/src/app/events/sdk_message.rs:1048-1058
**`handle_stream_event` and `handle_unknown` are no-op functions** — Found by code-simplifier (high). Replace match arms with `Message::StreamEvent { .. } | _ => {}`.

### crates/forge-tui/src/app/connect/bridge_lifecycle.rs:118-138, 235-276
**`ConnectedEventData` struct exists only to thread one match arm** — Found by code-simplifier (high). 6-field struct + 8-line definition + 6-line forwarding; sibling `SessionReplaced` arm doesn't use one. Inline at the match site.

### crates/forge-tui/src/app/events/client.rs:71-86, 118-131, 155-168, 183-198, 199-214, 215-229, 287-291
**Stale-session check duplicated 7 times** — Found by code-simplifier (high). Same `if app.session_id.as_ref()...!= Some(session_id.as_str())` guard repeated 6+ times. Extract a helper or macro.

### crates/forge-sdk/src/client/runtime.rs:89, 193
**Reader-task error events silently dropped on closed events channel** — Found by silent-failure-hunter (medium). Transport I/O error or decode failure: `let _ = events_tx.send(Err(e))`. If receiver dropped (consumer torn down), error silently lost. `if events_tx.send(Err(e)).is_err() { tracing::warn!(...); }`.

### crates/forge-tui/src/app/notify.rs:124
**`notify_rust::Notification` failure silently ignored** — Found by silent-failure-hunter (medium). Desktop notification failures (D-Bus down, Notification Center disabled) silently swallowed. `tracing::debug!` on `Err`.

### crates/forge-agent/src/forge_sdk_worker.rs:73-76
**`current_dir()` chain hides cwd resolution issues** — Found by silent-failure-hunter (medium). Empty cwd later switches `load_history_updates` to global session scan instead of project-scoped. Log on each failure branch.

### crates/forge-tui/src/app/file_index.rs:524
**`let _ = for_each_candidate(...)` ignores cancellation/error** — Found by silent-failure-hunter (medium). Partial file-index can persist as if complete. Return `(Vec, bool)` or log cancellation.

### crates/forge-tui/src/app/slash/candidates.rs:101-104
**Slash autocomplete fails to detect `/` after non-ASCII whitespace** — Found by bug-hunter (high). `detect_slash_at_cursor` uses `line.find` (byte offset) then indexes `chars.get` (char index). For non-ASCII whitespace (NBSP, ideographic), positions diverge — silently fails to activate slash autocomplete. Use `line.char_indices().find(|(_, c)| !c.is_whitespace())`.

### crates/forge-tui/src/app/events/notices.rs:30-58
**Stale `turn_notice_refs` indices after standalone-notice removal** — Found by bug-hunter (medium). After `remove_standalone_notice` removes message at idx, all messages with index > idx shift down. Code only removes one entry from `turn_notice_refs`; others retain stale indices. The `dedup_key` check usually catches but is fragile. Decrement every `turn_notice_refs.location` index > removed_idx, or use a stable identifier.

### crates/forge-agent/src/translate/state_parsing.rs:110-112
**`attempt as u64` truncates negative values silently** — Found by bug-hunter (medium). `build_api_retry_update` casts f64 to u64 with `as`; negative saturates to 0. UI surfaces "API retry 0/0 after error, retrying in 0ms" if CLI sends bogus. Use `u64::try_from(value as i64).ok()?` or `value.max(0.0) as u64`.

### crates/forge-agent/src/userdata/settings.rs:124-135
**`unique_temp_path` collisions leak temp files on rename failure** — Found by bug-hunter (medium). `write_json_atomic` creates temp file then renames; if rename fails, temp file left on disk. Repeated failures accumulate `.settings.json.{nanos}.tmp` files. Cleanup on `Err` before propagating, or use `tempfile::NamedTempFile::persist_noclobber`.

---

## Possible Issues (low confidence)

### crates/forge-sdk/src/transport/process.rs:286
`recv().await.unwrap_or(Ok(None))` collapses panic-without-final-send and clean EOF. — silent-failure-hunter

### `account_info_from_shell()`
Returns `None` for every error (binary-missing, exit non-zero, malformed JSON). Misleads UI as "user logged out". — silent-failure-hunter

### crates/forge-tui/src/app.rs:84-107
Crossterm terminal mode setup/teardown errors silently ignored. — silent-failure-hunter

### crates/forge-agent/src/forge_sdk_worker.rs:45
`disconnect()` of previous client silently drops shutdown errors. — silent-failure-hunter

### crates/forge-sdk/src/client/runtime.rs (LP-1)
`disconnect()` doesn't await spawned `reader_loop` `JoinHandle`. — bug-hunter

### crates/forge-agent/src/forge_sdk_bridge.rs:272-292 (LP-2)
`cancel`, `set_mode`, `set_model` ignore the `_session_id` arg. — bug-hunter

### crates/forge-tui mention.rs (LP-3)
`chars[i + 1..cursor_col]` assumes `cursor_col <= chars.len()`. — bug-hunter

### dispatch / new_session / resume_session (LP-4)
Ignored `tokio::spawn` `JoinHandle`s. — bug-hunter

### Forward-API surfaces
- `SubagentDefinition::with_*` builder family largely unused (forward API). — dead-code-analyzer
- Many `OptionsBuilder` methods unused in production (test-only); tracked partially by issue #37. — dead-code-analyzer
- `Client::rewind_files` only test-driven (couples to F9/F10). — dead-code-analyzer
- `ToolOutputBlock` single-variant enum (forward-compat). — code-simplifier
- `forge-tui/src/agent.rs` re-export shim (~40 lines). — code-simplifier
- TUI ↔ primitives parallel struct hierarchies for `ToolCall` etc. (some justification). — code-simplifier
- `PendingResponses` / `PendingQuestions` type aliases (probably earn keep). — code-simplifier
- `SessionStartReason::Logout` shape choice. — code-simplifier

---

## Notes for triage

- The two **critical** findings are both single-mechanism fixes: switch the relevant `Mutex` slots to `parking_lot::Mutex`. `parking_lot` is already in workspace deps. Low-risk, high-impact — strong candidate for the next commit.
- The **parallel type universe** trio (architecture-reviewer Finding A, code-simplifier #2, code-simplifier #3) all converge on the same lift: consume `forge_primitives` directly from forge-tui. Open a tracking issue (or attach to existing #38) and stage the migration in batches of 3-5 byte-equivalent types.
- The **catalog mutations + scan** dead-code findings (~250 LoC half-wired) need a product decision before code action: wire up session delete/tag/fork, or drop. Not safe to silently delete — `Command::RewindFiles` log line suggests intent.
- No conflicts between specialists this round.
