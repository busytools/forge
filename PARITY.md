# Python SDK parity tracking

This file is the single source of truth for **where forge-sdk is** relative to Python `claude-agent-sdk` upstream. Every parity-check run (weekly — see below) updates this file.

## Current state

| Field | Value |
|---|---|
| **Target Python SDK version** | `0.1.64` (released 2026-04-20) |
| **Target Python SDK commit** | `anthropics-claude-agent-sdk-python-1267352` (tarball SHA prefix) |
| **forge-sdk version at parity** | `v0.1.64` (deep audit pass) |
| **Last full parity run** | 2026-04-21 (deep dive, second pass same day) |
| **Next parity check due** | 2026-04-27 (first weekly) |
| **Versioning convention** | forge-sdk version mirrors the Python SDK release it targets — `v0.1.64` means parity with Python v0.1.64. No separate forge-sdk patch numbers. |
| **Design-spec basis** | `~/.claude-stargate/plans/2026-04-21-forge-sdk-port-design.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-m0-m1-plan.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-m2-m3-plan.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-m4-m7-plan.md` + `~/.claude-stargate/plans/2026-04-21-forge-sdk-corrections.md`. |

## Parity log

Each entry below records one weekly parity check.

### 2026-04-22 — API surface + field coverage follow-up (`parity-followup` branch)

Second pass after the `parity-wire-fixes` drop — closes the non-wire
items flagged in `audits/2026-04-22-parity-review.md`.

- **`Client::receive_response`** — drain `next_event` until (and
  including) a `Message::Result` and return the `Vec<Message>`.
  Python's `client.py:566-605`, the most-used convenience helper.
- **`Client::set_model(Option<&str>)`** — Python `client.py:345-367` +
  `_internal/query.py:688-695`. None serialises as JSON `null` to
  revert to the CLI default.
- **`Client::get_server_info() -> Option<&Value>`** — returns the
  cached `initialize` response (capabilities, commands, output
  styles). Required capturing the handshake body on the Client
  struct — Python stores the same at `_internal/query.py:214`.
- **Subagent parity fix.** `list_subagents` now returns `Vec<String>`
  of agent IDs (was `Vec<SDKSessionInfo>`) and recursively walks
  `subagents/` for `agent-<id>.jsonl` files (was `<id>.jsonl`, which
  never matched real transcripts). `get_subagent_messages` gains
  `limit` / `offset` parameters mirroring Python.
- **Message field coverage.**
  - `Message::Assistant` gains `error: Option<AssistantMessageError>`
    (new `snake_case` enum mirroring Python's `Literal` union) and
    `uuid: Option<String>`.
  - `AssistantEnvelope::usage` → `Option<Usage>` (Python reads as
    `data["message"].get("usage")`).
  - `Message::User` gains `uuid: Option<String>` and
    `tool_use_result: Option<Value>`.
  - `ToolPermissionContext` gains `suggestions: Vec<PermissionUpdate>`
    (populated from the control_request's `permission_suggestions`
    via typed decode) and `signal: Option<Value>` (Python's future
    abort-signal placeholder).
- **Option aliases + polish.** `OptionsBuilder::cli_path` ergonomic
  alias for `.binary()`; `PreCompactInput::custom_instructions` gets
  `skip_serializing_if`; `RateLimitInfo::raw` catches unknown CLI
  fields via `serde(flatten)`; `HookSpecificOutput` gains a
  clarifying doc note about caller-ergonomics role; dead public
  surface downgraded (`sanitize_path_public` /
  `validate_uuid_public` to `pub(crate)`; `build_args_legacy`
  deleted).

**Test count:** 251 tests + 3 ignored pass on `just check` (12 new
across this drop — see `result_message_fields.rs`,
`message_extras.rs`, `stream_event_and_error_frames.rs`, the expanded
`control_subtypes.rs`, and new in-module subagent tests).

**Still outstanding** (tracked for follow-up): `Error::MessageParse { data }`
field expansion (40+ construction sites — its own commit); the
session-module cluster collapse (wants a version bump — breaks
downstream imports); top-level `query()` signature alignment; and
ongoing Tier 6 test mirroring (currently 1 / 27 upstream files —
`errors.rs` only).

### 2026-04-22 — wire-protocol + API coverage fixes (`parity-wire-fixes` branch)

Follow-up to the 2026-04-22 parity + architecture review
(`audits/2026-04-22-parity-review.md`). Six verified wire-protocol
bugs the earlier sweeps had missed, each shipped with regression
tests against Python v0.1.64.

- **C1 — `stream_event` + `error` frames rejected.** `codec.rs::decode_dispatch`
  rejected both as unknown `type`. The CLI emits `stream_event`
  whenever `Options::include_partial_messages` is set
  (`types.py:1043-1050` + `message_parser.py:229-240`), and Python
  injects synthetic `{"type":"error","error":...}` frames at
  `_internal/query.py:315` when the read loop fails. Added
  `Message::StreamEvent { uuid, session_id, event, parent_tool_use_id }`
  and `Message::Error { error }` + matching codec dispatch.
- **C2 — `Message::Result` missing 7 fields; `total_cost_usd` mis-typed.**
  Rust had 8 fields and required `total_cost_usd: f64` + `usage:
  Usage`. Python `data.get(...)` (`_internal/message_parser.py:205-227`)
  leaves both optional — any free-tier or error-path result frame
  would serde-reject. Now matches Python `ResultMessage`
  (`types.py:1023-1039`): `total_cost_usd: Option<f64>`, `usage:
  Option<Usage>`, plus new optional fields `stop_reason`, `result`,
  `structured_output`, `model_usage` (camelCase wire key
  `modelUsage`), `permission_denials`, `errors`, `uuid`.
- **C3 — `SandboxSettings`, `SandboxNetworkConfig`, `SandboxIgnoreViolations`
  had fabricated field names.** Every field was Rust-side invention
  that the CLI doesn't recognise — the `--settings` JSON merge at
  `options.rs:360-365` produced a shape the binary silently ignores.
  Rewritten to Python v0.1.64 (`types.py:782-856`): `enabled`,
  `autoAllowBashIfSandboxed`, `excludedCommands`,
  `allowUnsandboxedCommands`, `network` (with `allowUnixSockets` /
  `allowAllUnixSockets` / `allowLocalBinding` / `httpProxyPort` /
  `socksProxyPort`), `ignoreViolations` (with `file` / `network`),
  `enableWeakerNestedSandbox`.
- **C4 — `Client::fork_session` phantom `control_request` subtype.**
  Python has no runtime `fork_session` subtype; the method was
  sending a request the CLI cannot handle and would always error in
  production. Removed. Session forking lives in `Options::fork_session`
  (spawn-time → `--fork-session`) and the offline
  `session_mutations::fork_session` function.
- **C5 — `--system-prompt ""` suppression silently dropped.** Python
  (`subprocess_cli.py:209-210`) always emits one of the
  `--system-prompt{,-file}` / `--append-system-prompt` forms —
  including an explicit empty string when the option is unset, so the
  CLI doesn't fall back to its builtin prompt. forge-sdk now matches
  byte-for-byte.
- **C6 — Hook callback response dropped `SyncHookJSONOutput` control
  fields.** Python (`types.py:463-505` + `query.py:40-55`) forwards
  `continue_`/`suppressOutput`/`stopReason`/`systemMessage` from the
  callback to the CLI with `continue_` → `continue` wire rename;
  forge-sdk's `HookDecision` modelled none of them, so a Python
  callback that halted the agent via `continue: false` did nothing
  in Rust. Added four builder methods (`with_continue`,
  `with_suppress_output`, `with_stop_reason`, `with_system_message`)
  + accessors + wire emission in `handle_hook_callback`.

**Test count:** 239 tests + 3 ignored pass on `just check` (14 new
across C1-C6). The parity-review report at
`audits/2026-04-22-parity-review.md` catalogues all findings
including the medium / minor / architectural items not touched in
this drop (missing `Client::set_model` / `get_server_info` /
`receive_response`; the session-module cluster reshape; dead public
surface; etc.).

### 2026-04-22 — audit follow-up + lite read + test mirror kickoff (`audit-followup-parity` branch)

Follow-up session after the 2026-04-22 audit landed (`c4df432`). Picked
up the deferred structural items and the last Tier-5 parity
simplification, plus kicked off Tier 6 (test mirroring) with the
contract PARITY.md already describes.

- **I18 — `hooks.rs` split.** 1035-LoC monolith broken into
  `hooks/inputs.rs`, `hooks/outputs.rs`, `hooks/callback.rs`, and
  `hooks/registry.rs`. Parent keeps `HookKind` + `HookContext` + the
  public re-exports; no consumer-visible change.
- **I5 — `client.rs` split.** 854-LoC client extracted into
  `client/control_dispatch.rs` (inbound dispatch — `handle_control`,
  `handle_mcp_message`, `handle_hook_callback`,
  `write_unsupported_control_error`) and `client/control_send.rs`
  (outbound — `send_control` + 9 typed wrappers + 2 `_raw` escape
  hatches). Parent keeps the struct, `Debug` impl, spawn / next_event
  / transcript-mirror plumbing / disconnect. Per-HookKind
  `hookSpecificOutput` encoding moved into
  `hooks/outputs::encode_updated_input_wrapper` next to the typed
  output structs — `handle_hook_callback` no longer carries inline
  per-event wrapper logic.
- **`_read_session_lite` head/tail optimisation.** `list_sessions`
  / `get_session_info` now read at most 64 KiB from each end of a
  JSONL file rather than scanning the whole thing — matches Python
  `_read_session_lite` / `_parse_session_info_from_lite` byte
  semantics. The port also closes behavioural gaps the full-scan
  path had: sidechain-session skip, metadata-only-session skip,
  `aiTitle` / `lastPrompt` fallbacks, head-first `cwd` / tail-first
  `gitBranch`, and tag extraction scoped to `{"type":"tag"}` lines
  (prevents `git tag` tool_use inputs from being picked up as
  session tags — reproduced in a new unit test).
- **Tier 6 kickoff — `tests/python_parity/` populated with the first
  mirror.** Strategy: `tests/python_parity.rs` as the test-binary
  entry + `tests/python_parity/<file>.rs` submodules referenced via
  `#[path]`. First port: `errors.rs` mirrors
  `tests/test_errors.py` (4 of 5 tests mapped to Rust `Error` enum
  variants, 1 explicitly skipped with rationale — Rust enum has no
  bare-message base variant). All upstream tests now have a tagged
  Rust counterpart or a documented skip stub.

**Simplifications resolved in this pass (remove from "still simplified"):**
- `_read_session_lite` head-only optimisation — done.

**Still simplified relative to Python:**
- Git-worktree discovery for `list_sessions(include_worktrees=true)` —
  accepted as a parameter but ignored (single-project scan only). *Correction
  from prior entry:* `d1e7610` wires the porcelain lookup; the backlog item
  here now covers fully honouring per-worktree project dirs.
- `fork_session` auto-title — falls back to `(no title)` rather than
  deriving `<original> (fork)` when no explicit title is passed.
- `list_subagents` + `get_subagent_messages` return type / filename
  divergence — Python returns `list[str]` of agent IDs and expects
  `agent-<id>.jsonl`; forge-sdk returns `Vec<SDKSessionInfo>` and reads
  `<id>.jsonl`. Leftover from the initial port; flagged during the lite
  read work.

**Test count:** 225 tests + 3 ignored pass on `just check` (fmt + clippy
all-targets `-D warnings` + nextest forge-sdk + docs `-D warnings`).
17 new unit/integration tests (12 in `sessions.rs` covering the lite
read + 5 in `tests/python_parity/errors.rs`). Python SDK `tests/`
mirror coverage: 1 / ~27 upstream test files (~3.7%).

### 2026-04-22 — major surface sweep (`parity-tier1-wire-risk` branch)

Single-session push through Tiers 1–5 of the 2026-04-21 handoff.
Branch `parity-tier1-wire-risk` landed 17 commits covering:

- **Tier 1 (wire-risk):** all 7 items — `BaseHookInput` flatten, 4
  missing hook input types + typed `hookSpecificOutput` wrappers,
  `control_cancel_request` inbound handling, `rate_limit_event`,
  task-lifecycle frames (started/progress/notification), and
  `mirror_error` frames. forge-sdk no longer drops any frame the CLI
  currently emits.
- **Tier 2 (behavioural parity):** `TranscriptMirrorBatcher` with
  coalesce + 500-entry / 1-MiB eager flush + `MirrorError` on-channel
  emission; `PermissionUpdate` (6 variants) attached to allow
  decisions via `with_updated_permissions`; `AgentDefinition` + the
  `agents` field on the initialize control_request.
- **Tier 3 (option surface):** ~25 new options wired end-to-end:
  system_prompt / tools / disallowed_tools / max_turns /
  max_budget_usd / fallback_model / betas / continue_conversation /
  session_id / include_partial_messages / fork_session / add_dirs /
  plugins / env / user / extra_args / effort / thinking /
  max_thinking_tokens / task_budget / output_format /
  max_buffer_size / stderr / load_timeout_ms /
  enable_file_checkpointing / settings + sandbox (with JSON merge).
  `build_args(&Options)` pure function exposed for argv inspection;
  25 tests exercise flag-by-flag parity.
- **Tier 4 (public types):** `public_types.rs` module with
  `SettingSource`, `SdkBeta`, `StreamEvent`, `SDKSessionInfo`,
  `SessionMessage(Kind)`, `McpServerConnectionStatus`,
  `McpToolAnnotations`, `McpToolInfo`, `McpServerInfo`,
  `McpServerStatus`, `McpStatusResponse`, `ContextUsageCategory`,
  `ContextUsageResponse`, `McpServerConfig` (Stdio/Sse/Http),
  `SandboxSettings` + `SandboxNetworkConfig` +
  `SandboxIgnoreViolations`. `InMemorySessionStore` alias added for
  Python surface parity. `mcp_status()` and `get_context_usage()`
  return typed; `_raw()` escape hatches retained.
- **Tier 5 (helpers):** top-level `query()` one-shot; offline
  `list_sessions` / `get_session_info` / `get_session_messages`;
  `rename_session` / `tag_session` / `delete_session` (JSONL
  append / file removal). Includes a JS-style 32-bit path-sanitise
  hash (for project-key compat with the CLI) and a minimal no-dep
  ISO-8601 parser.

**All 6 Tier-5 helpers are now in:**
- `list_sessions` / `get_session_info` / `get_session_messages`
- `list_subagents` / `get_subagent_messages`
- `rename_session` / `tag_session` / `delete_session` /
  `fork_session` + `ForkSessionResult`
- `*_from_store` / `*_via_store` async variants of every helper above
- `project_key_for_directory`, `InMemorySessionStore` alias

`fork_session` does proper UUID remap (`uuid` crate, `v4` feature) +
optional `up_to_message_id` boundary + optional custom-title attach.

**Still simplified relative to Python:** (superseded — see the
2026-04-22 follow-up entry above. `_read_session_lite` is now ported;
`list_subagents` / `get_subagent_messages` divergence documented.)

**Test count:** 208 tests + 2 ignored pass on `just check` (fmt +
clippy all-targets -D warnings + nextest forge-sdk + docs -D
warnings). Python SDK's own `tests/` directory not yet mirrored
(Tier 6, ongoing).

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

### 2026-04-21 — Python SDK v0.1.64 (deep-dive, second pass)

This entry is the exhaustive audit. The first-pass entry below (same
day) checked three items; this one walks every file in the Python SDK
and catalogues every divergence.

- **Upstream target:** v0.1.64 (tarball `anthropics-claude-agent-sdk-python-1267352`).
- **Surface examined:**
  - `types.py` (1553 LoC) — every public type + control-request union.
  - `_errors.py` (56 LoC) — exception hierarchy.
  - `_cli_version.py` (3 LoC) — `__cli_version__ = "2.1.116"`.
  - `__init__.py` (643 LoC) — public re-export surface (~120 names).
  - `_internal/query.py` (825 LoC) — control dispatch + stdio loop.
  - `_internal/transport/subprocess_cli.py` (736 LoC) — CLI argv.
  - `_internal/transcript_mirror_batcher.py` — batcher semantics.
  - `_internal/session_store.py` + `_internal/sessions.py` — session helpers.
  - `_internal/session_mutations.py` — fork/rename/tag/delete.
  - `client.py` + `_internal/client.py` — public `ClaudeSDKClient`.
  - `query.py` — top-level one-shot `query()` function.

#### Wire divergences FIXED in v0.1.64

- **Outbound `stop_task` payload** — Python `types.py:1519` expects
  `{"subtype":"stop_task","task_id":"..."}`. forge-sdk was sending
  `tool_use_id` (wrong name, wrong semantic — Python's `task_id` is
  the sub-agent's task id, not a tool-use id). FIXED —
  `Client::stop_task(task_id: &str)`.
- **Outbound `rewind_files` payload** — Python `types.py:1497`
  requires `user_message_id: str`. forge-sdk was sending an empty
  object. FIXED — signature is now
  `Client::rewind_files(user_message_id: &str)`.
- **Outbound `mcp_reconnect` / `mcp_toggle` field name** — Python
  `types.py:1505`, `1513` uses camelCase `serverName`. forge-sdk was
  sending snake_case `server_name`. FIXED — both now emit
  `"serverName"`.
- **CLI argv — spurious `--input-format`** — Python
  `subprocess_cli.py:207` only emits `--output-format stream-json
  --verbose`. forge-sdk added `--input-format stream-json`. Dropped
  to match argv byte-for-byte.
- **CLI argv — always-on `--permission-mode`** — Python emits
  `--permission-mode` only when the caller sets one explicitly
  (`subprocess_cli.py:267-268`). forge-sdk always passed
  `--permission-mode default`. Fixed — now conditional on
  `permission_mode != Default`.

#### Major surface gaps flagged (not regressions, scope expansions)

The Python SDK exposes types and options forge-sdk doesn't model yet.
None are wire-breaking for the Client paths we already ship, but they
are real parity gaps against the "every public type in Python has a
Rust counterpart" invariant.

- **Options on `ClaudeAgentOptions` not exposed in `OptionsBuilder`:**
  `tools`, `disallowed_tools`, `system_prompt` (with preset/file
  variants), `continue_conversation`, `session_id`, `max_turns`,
  `max_budget_usd`, `fallback_model`, `betas`, `cli_path`, `settings`,
  `add_dirs`, `env`, `extra_args`, `max_buffer_size`, `stderr`
  callback, `user`, `include_partial_messages`, `fork_session` (as
  spawn-time flag; we have a runtime method), `agents`, `sandbox`,
  `plugins`, `max_thinking_tokens`, `thinking`, `effort`,
  `output_format`, `enable_file_checkpointing`, `load_timeout_ms`,
  `task_budget`.
- **Public types not ported:** `AgentDefinition`, `PermissionUpdate` +
  `PermissionRuleValue`, `RateLimitInfo` + `RateLimitEvent`,
  `TaskStartedMessage` + `TaskProgressMessage` +
  `TaskNotificationMessage` + `TaskUsage` + `TaskBudget`,
  `MirrorErrorMessage`, `SandboxSettings` (+ `NetworkConfig` +
  `IgnoreViolations`), `SdkPluginConfig`, `ThinkingConfig` (+ 3
  variants), `StreamEvent`, `SystemPromptPreset` + `SystemPromptFile`
  + `ToolsPreset`, `SdkBeta`, `ContextUsageCategory` +
  `ContextUsageResponse` (we return raw `Value`), `McpServerStatus` +
  variants (we don't model `get_mcp_status` response),
  `McpServerConfig` with stdio/sse/http variants (we only model
  in-process "sdk"), `SDKSessionInfo`, `SessionMessage`.
- **Hook inputs missing `BaseHookInput` fields:** Python hooks flatten
  `session_id`, `transcript_path`, `cwd`, `permission_mode` into every
  hook input. forge-sdk's hook inputs (`PreToolUseInput`, etc.) only
  carry the event-specific fields. Add a `BaseHookInput` mixin
  analogue.
- **Missing hook input types:** `PermissionRequestHookInput`,
  `NotificationHookInput`, `SubagentStartHookInput`,
  `PostToolUseFailureHookInput`. We have the `HookKind` enum variants
  but no typed input structs and no builder methods on `HooksBuilder`.
- **Typed `hookSpecificOutput` wrappers:** Python ships
  `PreToolUseHookSpecificOutput`, `PostToolUseHookSpecificOutput`,
  `NotificationHookSpecificOutput`, etc. forge-sdk builds the JSON
  inline in `handle_hook_callback`. Land typed wrappers.
- **Offline session helpers not exposed:** `list_sessions`,
  `get_session_info`, `get_session_messages`, `list_subagents`,
  `get_subagent_messages` (+ `_from_store` async variants).
  `project_key_for_directory` is now present as
  `session_store::sanitise` but not re-exported under the Python name.
- **Session mutations via-store not exposed:** `rename_session`,
  `tag_session`, `delete_session`, `fork_session_via_store`, etc.
- **Error type flat vs. hierarchical:** Python has `ClaudeSDKError` →
  `CLIConnectionError` → `CLINotFoundError`, plus sibling
  `ProcessError`, `CLIJSONDecodeError`, `MessageParseError`. forge-sdk
  uses one enum. Acceptable Rust idiom, but note that Python's
  `MessageParseError` carries a `data: dict | None` field our variant
  lacks.
- **`control_cancel_request` inbound handling:** Python
  `_internal/query.py:274-280` cancels the in-flight control handler
  when the CLI sends a cancel. forge-sdk doesn't yet.
- **Transcript-mirror batcher:** Python's `TranscriptMirrorBatcher`
  coalesces by `filePath`, eager-flushes at 500 entries or 1 MiB,
  explicit-flushes on `result` arrival with a 60 s per-append timeout,
  and reports failures through an `on_error` callback that emits a
  `mirror_error` system message. forge-sdk has a per-frame append
  stub; flush-on-result is wired as a no-op hook ready for mechanical
  swap.
- **MCP config type beyond SDK:** Python accepts in-process (`type: sdk`),
  `stdio`, `sse`, `http` configs. forge-sdk only models in-process.
- **`query.py` top-level helper** — Python exposes a one-shot
  `async query(prompt, options) -> AsyncIterator<Message>`. forge-sdk
  has only the `Client::spawn` builder surface.
- **`can_use_tool` callback — `updated_permissions`:** Python's
  `PermissionResultAllow` carries an `updated_permissions:
  list[PermissionUpdate] | None` field. forge-sdk's `PermissionDecision`
  only models `updated_input`.

#### CLI argv flags not yet emitted

The following Python argv flags are not currently emitted by
forge-sdk (their option fields don't exist yet): `--system-prompt`,
`--system-prompt-file`, `--append-system-prompt`, `--tools`,
`--disallowedTools`, `--max-turns`, `--max-budget-usd`,
`--fallback-model`, `--betas`, `--continue`, `--session-id`,
`--settings`, `--add-dir`, `--include-partial-messages`,
`--fork-session` (spawn-time flag vs. control_request),
`--plugin-dir`, `--task-budget`, `--thinking`,
`--max-thinking-tokens`.

#### Ported in forge-sdk this pass

- Commits on branch `parity-v0.1.64-fixes` (pending push):
  - Wire fixes for `stop_task`, `rewind_files`, `mcp_reconnect`,
    `mcp_toggle`.
  - CLI argv: drop `--input-format`, gate `--permission-mode` on
    non-default variant.
  - Version bump to `0.1.64` (matches Python exactly per the
    mirrored-versioning convention).

#### Still outstanding (tracked for next weekly)

Priority order for 2026-04-27 and following weeks:

1. **`BaseHookInput` flattening** on all hook input types + add
   missing `PermissionRequestHookInput`, `NotificationHookInput`,
   `SubagentStartHookInput`, `PostToolUseFailureHookInput` types.
2. **Typed `hookSpecificOutput` wrappers** per event.
3. **Full `TranscriptMirrorBatcher`** (500-entry / 1-MiB eager flush,
   explicit flush on result, `on_error` → `MirrorErrorMessage`
   synthesis).
4. **`control_cancel_request` inbound handling.**
5. **`AgentDefinition`** + `OptionsBuilder::agents()` + populate the
   `initialize` payload properly.
6. **`RateLimitEvent` + `RateLimitInfo`** — CLI emits these; we drop
   them as unknown frames.
7. **`Task*` system messages** — same.
8. **Offline session helpers** (`list_sessions`, etc.).
9. **Session mutations via-store** (`rename_session`, etc.).
10. **`McpServerConfig` variants** (stdio/sse/http) beyond in-process.
11. **`PermissionUpdate`** on `PermissionDecision::allow_with_input`.
12. **Sandbox, ThinkingConfig, SdkPluginConfig** options.
13. Start Python `tests/` → `crates/forge-sdk/tests/python_parity/`
    mirror.

#### Test-mirror status

- Mirror suite: `crates/forge-sdk/tests/python_parity/` — empty.
- Coverage %: 0 / ~30 upstream test files.
- Skip count: 0.

#### forge-sdk tag released

`v0.1.64` — version mirrors Python exactly.

#### Notes for 2026-04-27

- Upstream is still at v0.1.64. First item on that run: diff for
  new releases.
- Consider scoping the backlog items into a v0.1.64.X patch
  sequence for forge-sdk (e.g. `v0.1.64-1`, or track forge-sdk
  patches against the same Python version).
- The "every public type mirrored" backlog is large; prioritise by
  wire-protocol risk (BaseHookInput + hookSpecificOutput first,
  types second).

---

### 2026-04-21 — Python SDK v0.1.64 (first run, superseded by deep-dive above)

Kept for history — a narrower first pass that covered three items.

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
