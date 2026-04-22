# 2026-04-22 — parity + architecture review

Second review pass after the audit follow-up and Tier 1–6 parity work
landed (`5b401d4`). Three parallel investigations:

1. **Parity audit** — public-surface + wire-shape diff against Python
   `claude-agent-sdk` v0.1.64.
2. **Architecture review** — internal code quality sweep.
3. **Rust style survey** — comparison against user's Rust idioms in
   hub-modules, aware, granite-backend, subspace.

Findings verified where severity warranted; four of the top-six
critical claims checked directly against Python source and code.

## Top-line verdict

- **forge-sdk's internal shape and Rust idiom are in good order.** The
  style survey finds 10 consistent patterns across your other Rust
  projects; forge-sdk matches all of them. Four deviations are
  justified (error chain via `#[source]`, type-erasure for callbacks,
  macros for schema, Options mutability).
- **Wire-protocol and public-surface parity has several real bugs the
  handoffs missed.** Six are concrete — these will break real CLI
  interactions for specific feature sets (`include_partial_messages`,
  sandbox config, `fork_session` runtime method, result-frame
  decoding).
- **Test mirror coverage is 1 / 27 files.** The critical-path
  upstream tests (`test_message_parser.py`, `test_transport.py`,
  `test_tool_callbacks.py`, `test_transcript_mirror.py`) are all
  empty — without them there is no automated regression gate for the
  wire bugs flagged here.
- **The single biggest architectural reshape** is the four session
  modules (`sessions.rs`, `sessions_store.rs`, `session_store.rs`,
  `session_mutations.rs`). The singular/plural name collision costs
  reviewer minutes. Not a bug, but the highest-signal pre-review
  cleanup.

## Critical — wire-protocol or correctness bugs

These are breakage, not style. Each is reproducible against a real
CLI session. All verified against Python v0.1.64 source.

### C1. `stream_event` and `error` frames are rejected

- **Where:** `crates/forge-sdk/src/transport/codec.rs:98` —
  `"assistant" | "user" | "system" | "result" | "rate_limit_event"`
  is the only allowlist.
- **Python:** `_internal/message_parser.py:229-240` parses
  `stream_event` into typed `StreamEvent`;
  `_internal/query.py:315` injects `{"type": "error", "error": ...}`
  into the message stream on transport errors.
- **Impact:** Any session with `include_partial_messages=true` (an
  option we already emit) produces CLI frames forge-sdk rejects as
  unknown type. Transport errors from Python side likewise surface
  as `Error::MessageParse` rather than propagating through.
- **Fix sketch:** Add `stream_event` and `error` arms to the match.
  Add a `Message::StreamEvent { ... }` variant (already a public
  type in `public_types.rs::StreamEvent`) and a `Message::Error
  { error: String }` variant. Update `messages.rs::Message` + the
  deserializer.

### C2. `Message::Result` is missing 7 fields and mis-typed

- **Where:** `crates/forge-sdk/src/messages.rs:140-157`. 8 fields
  present, including `total_cost_usd: f64` (non-Option).
- **Python:** `types.py:1023-1033` declares 13 fields:
  `subtype`, `session_id`, `is_error`, `num_turns`, `duration_ms`,
  `duration_api_ms`, `total_cost_usd: float | None`, `usage`,
  `stop_reason`, `result`, `structured_output`, `model_usage`,
  `permission_denials`, `errors`, `uuid`.
- **Impact:** serde will REJECT any CLI result frame that doesn't
  carry `total_cost_usd` (free-tier or error-path result frames).
  The dropped fields silently lose information — callers inspecting
  `Message::Result` never see `stop_reason` or `errors`.
- **Fix sketch:** Change `total_cost_usd` to `Option<f64>` with
  `#[serde(default)]`. Add the 7 missing fields as
  `Option<...>` with `#[serde(default)]`. Needs corresponding
  types for `ModelUsage`, `PermissionDenial` etc.

### C3. `SandboxSettings`, `SandboxNetworkConfig`, `SandboxIgnoreViolations` — every field name is wrong

- **Where:** `crates/forge-sdk/src/public_types.rs:318-368`.
- **Rust fields:** `extra_read_only_paths`, `extra_read_write_paths`,
  `exclude_tools`, `allowed_hosts`, `denied_hosts`, `paths`,
  `protocols`.
- **Python fields** (verified from v0.1.64 `types.py:782-856`):
  `enabled`, `autoAllowBashIfSandboxed`, `excludedCommands`,
  `allowUnsandboxedCommands`, `network`, `ignoreViolations`,
  `enableWeakerNestedSandbox`. Network: `allowUnixSockets`,
  `allowAllUnixSockets`, `allowLocalBinding`, `httpProxyPort`,
  `socksProxyPort`. Violations: `file`, `network`.
- **Impact:** The `--settings` JSON merge at `options.rs:360-365`
  produces a `{"sandbox": <rust-shaped>}` blob the CLI cannot parse.
  Sandbox is completely non-functional on the wire.
- **Fix sketch:** Rewrite all three structs to match Python's
  shapes. Every field is wrong — treat as a full rewrite.

### C4. `Client::fork_session` sends a control_request Python never defines

- **Where:** `crates/forge-sdk/src/client/control_send.rs:247-264`
  — issues `{"subtype": "fork_session"}`.
- **Python:** no matching control_request subtype. `fork_session`
  exists in two unrelated places: the spawn-time option
  `Options::fork_session: bool` (→ `--fork-session` flag at
  `subprocess_cli.py:319`) and the offline
  `session_mutations::fork_session()` function.
- **Impact:** Calling `Client::fork_session(...)` always errors at
  the CLI.
- **Fix sketch:** Remove the runtime method. The existing offline
  `fork_session` free function already serves the real use case.
  Document the removal in PARITY.md.

### C5. `--system-prompt ""` suppression is silently dropped

- **Where:** `crates/forge-sdk/src/argv.rs:34-50` — skips the flag
  when `Options::system_prompt` is `None`.
- **Python** (verified from `_internal/transport/subprocess_cli.py:209-210`):
  always emits `--system-prompt ""` in that case.
- **Impact:** Default-case argv diverges by 2 tokens. Semantically
  forge-sdk gets whatever system prompt the CLI considers default,
  rather than the empty-string override Python sets.
- **Fix sketch:** emit `--system-prompt ""` when
  `system_prompt.is_none()`. Update `tests/argv_composition.rs`.

### C6. Hook callback response drops most `SyncHookJSONOutput` fields

- **Where:** `crates/forge-sdk/src/client/control_dispatch.rs:223-234`.
- **Python** (`_internal/query.py:395-396`): ships the full caller
  dict including `continue_`, `suppressOutput`, `stopReason`,
  `systemMessage`.
- **Impact:** Hook callers can't signal "stop" or attach system
  messages through forge-sdk. A hook that would halt the agent in
  Python is a no-op in Rust.
- **Fix sketch:** Extend `HookDecision` with optional `continue_`,
  `suppress_output`, `stop_reason`, `system_message` fields.
  Propagate through `handle_hook_callback`.

## Major — public API coverage gaps

### Missing `Client` methods

- **`Client::set_model`** — Python `client.py:345-367` +
  `_internal/query.py:688-695`. Switches the model mid-session.
- **`Client::get_server_info`** — Python `client.py:541-564`.
  Returns cached initialize response (capabilities, server name).
- **`Client::receive_response`** — Python `client.py:566-605`.
  Yields messages until `ResultMessage`, then stops. The single
  most-used convenience method in Python examples. Rust's
  `next_event` yields indefinitely; callers reimplement this by
  hand.

### Missing message fields

- `AssistantEnvelope` missing `error`, `uuid`, `message_id`
  (`messages.rs:161-179` vs `types.py:917-929`). The typed
  `AssistantMessageError` union (`"authentication_failed"` |
  `"billing_error"` | `"rate_limit"` | `"invalid_request"` |
  `"server_error"` | `"unknown"`) is invisible to Rust callers.
- `UserMessage` missing `tool_use_result`, `uuid`
  (`messages.rs:181-188` vs `types.py:906-913` +
  `message_parser.py:56-57,85-87`).
- `ToolPermissionContext` missing `suggestions`, `signal`
  (`permissions.rs:14-28` vs `types.py:175-186`). The decoder
  already captures `permission_suggestions` on
  `ControlRequestKind::CanUseTool` but discards it before the
  callback sees it.

### Top-level `query()` diverges

- Python `query.py:11-16`: `query(*, prompt, options=None,
  transport=None) -> AsyncIterator`.
- Rust `lib.rs:114`: `query(prompt, options) -> Vec<Message>`.
- **Impact:** Both positional vs keyword and iterator vs Vec.
  Ergonomic for Rust, but callers bridging between the two SDKs
  have to adapt.

### Other misses

- `Options::cli_path` missing (Python alias for `binary`;
  ergonomic only).
- `Options::debug_stderr` missing (deprecated but still present in
  Python).

## Minor / cosmetic

- **`argv.rs:24-28` comment self-contradicts the code.** Comment
  says `--input-format` is dropped for Python parity; line 237
  pushes it. Either remove the flag or fix the comment — one or
  the other is stale.
- **`PreCompactInput::custom_instructions`** lacks `skip_serializing_if = "Option::is_none"`
  at `hooks/inputs.rs:159-160`. Minor serialisation noise.
- **`RateLimitInfo::raw: dict[str, Any]`** field missing. Python
  echoes the full CLI payload for forward compat; Rust decodes
  only known fields.
- **No `testing/` submodule.** Python ships
  `testing.session_store_conformance` for adapter authors.
- **`HookSpecificOutput` untagged union** at `hooks/outputs.rs:211-230`
  is constructed but never pattern-matched. Add a one-line doc
  explaining it's for caller ergonomics, not internal dispatch.

## Architectural reshapes

### High-impact

**A1. Collapse the four session modules.** Single biggest review-
friction point. `sessions.rs` + `sessions_store.rs` +
`session_store.rs` + `session_mutations.rs` → `session/` directory
with `store.rs`, `scan.rs`, `mutations.rs`, `util.rs`. Eliminates
the `sessions_store.rs` vs `session_store.rs` singular/plural
collision. ~45 min, pure reorganisation, all behaviour preserved
via `pub use` from `session.rs`. Breaks downstream imports —
bundle with a version bump.

**A2. Remove dead public surface.**

- `sanitize_path_public` (`sessions.rs:40`) → `pub(crate)`. Zero
  external callers.
- `validate_uuid_public` (`session_mutations.rs:269`) → `pub(crate)`.
- `build_args_legacy` (`transport/process.rs:18-19`) → delete.
  `#[doc(hidden)]` but no callers.
- `Hooks::to_initialize_payload_for_test` — move behind
  `#[cfg(any(test, feature = "test-util"))]` or into a
  `pub(crate) mod testing`.

### Medium

- **`Error::MessageParse { reason, data: Option<Value> }`** —
  Python's `MessageParseError.data` field (flagged in outstanding
  backlog). Adding is a strict superset of today's shape; the 16
  call sites migrate with `data: None`.
- **`Message::From` impls in `messages.rs:432-667`** — 230 LoC
  of mechanical boilerplate around `TaskStarted`/`TaskProgress`/
  `TaskNotification`/`MirrorError`. A small `repr_dispatch!` macro
  reduces to ~30 lines. Defer if risk-averse.
- **Decision-kind accessors.** `PermissionDecision` and
  `HookDecision` hide their inner enum. A `#[non_exhaustive]
  pub enum DecisionKind` + `kind(&self) -> &DecisionKind`
  accessor lets callers exhaustively match. Defer if current
  test coverage is strong.
- **Builder asymmetry doc comment.** `OptionsBuilder` /
  `HooksBuilder` / `McpServerBuilder` / `AgentDefinition::with_*`
  all take `self` by value consistently — add a one-line
  comment at the top of `agents.rs` clarifying "struct-literal
  or `with_*` chain" are both canonical.
- **Async-trait vs `Pin<Box<dyn Future>>` rule.** The split is
  coherent (hand-roll for closure-friendly blanket impls, use
  `async_trait` for named-type impls) but undocumented. One
  comment block in `lib.rs` explaining the rule prevents future
  readers from flagging it as inconsistency.

### Deliberately not recommended

- Flat `src/*.rs` layout outside the session cluster — fine.
- Single `Error` enum vs Python's hierarchy — idiomatic Rust;
  keep.
- `PhantomData<fn() -> I>` on `ErasedCallback` — looks like
  over-abstraction; actually load-bearing for the blanket impl.
- `Client` with 8 `pub(crate)` fields split across `client.rs` +
  `client/{dispatch,send}.rs` — this is what good Rust looks
  like post-split.
- Two public MCP APIs (`McpServer` + `McpServerBuilder`) — keep.

## Test mirror priority (Tier 6)

Current: **1 / 27 files** (`errors.rs`). Priority order for the
next ports:

1. **`test_message_parser.py`** (877 lines) — the definitive
   wire-format spec. Would have caught C1, C2, C6 above.
2. **`test_transport.py`** (1987 lines) — subprocess lifecycle,
   argv, env. Would have caught C5.
3. **`test_tool_callbacks.py`** (849 lines) — `can_use_tool` +
   `updated_permissions` + `PermissionResult*`.
4. **`test_transcript_mirror.py`** (693 lines) — batcher
   semantics.
5. **`test_streaming_client.py`** (1314 lines) — `Client` lifecycle.
6. **`test_types.py`** (621 lines) — per-type field sets. Would
   have caught C3.

## Rust-style findings (style survey)

forge-sdk matches your consistent-across-projects Rust idioms
across all 10 patterns surveyed (nightly pinned, workspace deps,
thiserror+anyhow split, clippy forbid+deny, flat layout,
`#[non_exhaustive]`, builder+`#[must_use]`, tracing, tokio+channels,
plain `pub trait`). Four deviations are all justified by SDK
constraints:

1. **Error chain via `#[source]`** — forge-sdk preserves chains;
   aware uses flat `String` payloads for DB tag/retry queries.
   Keep both; they serve different use cases.
2. **Type-erasure via `Box<dyn Future>`** — SDK-specific (bridging
   JSON → Rust callback from subprocess).
3. **`macro_rules!` for schema declaration** — `tool!` macro and
   `declare_event_name_tag!` not a pattern in sibling projects;
   appropriate here for repetitive schema work.
4. **Long-lived `Options` mutability** — sibling projects are
   boot-and-go binaries; SDK is a session-manager with evolving
   state.

### Global-memory / TIL recommendations

- **Nothing warrants a new `~/.claude-granite/CLAUDE.md` entry.**
  The existing "style + idiom" list already captures the
  consistent patterns.
- **One TIL candidate from aware:** `error_type_tag()` +
  `is_retryable()` helpers on the error enum — pure functions that
  decompose errors into stable DB-storable tags and retry flags.
  Not yet in global memory; worth a cross-project TIL if you keep
  re-using the pattern. (No forge-sdk action.)

## Recommended sequencing

If doing anything before a merge/review:

**Must-fix (wire-protocol bugs, will break real sessions):**
- C1 (stream_event/error frames)
- C2 (Message::Result fields)
- C3 (Sandbox structs)
- C4 (fork_session phantom subtype — remove)
- C6 (hook response fields)

**Should-fix (argv divergence, argv.rs comment):**
- C5 (--system-prompt "" suppression)
- Minor #1 (argv comment contradicts code)

**Nice-to-have:**
- Missing `Client` methods (set_model, get_server_info, receive_response)
- Dead public surface downgrade (A2)

**Defer (wants version bump or larger effort):**
- Session module collapse (A1)
- Message field additions
- Top-level `query()` signature change

**Ongoing:**
- Tier 6 test mirror in the priority order above.
