# Changelog

All notable changes to `forge-sdk` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Version numbers mirror the Python SDK release they target parity with
(e.g. `forge-sdk 0.1.0` targets `claude-agent-sdk` v0.1.64+).

## [Unreleased]

## [0.1.2] — 2026-04-21

Closes the two remaining gaps called out under the v0.1.1 "Known gaps
still outstanding" section.

### Added

- **Integration tests for the 9 outbound control subtypes** —
  `interrupt`, `set_permission_mode`, `rewind_files`, `mcp_reconnect`,
  `mcp_toggle`, `stop_task`, `mcp_status`, `get_context_usage`,
  `fork_session`. New `mock_claude_control.sh` fixture handles the
  initialize handshake, then loops parsing each outbound
  `control_request` and emitting a matching `control_response` with a
  canned payload per subtype. Tests assert the round-trip returns the
  decoded payload for `mcp_status` / `get_context_usage` /
  `fork_session`.
- **`transcript_mirror` frame ingestion** — `Client::next_event` now
  intercepts `system/transcript_mirror` frames emitted by the CLI under
  `--session-mirror`, parses the `SessionKey` + `entries` payload, and
  calls `session_store.append(&key, &entries).await`. Frames are
  swallowed (never surface via `next_event`); parse failures and
  append errors are logged at `warn!` and continue the event loop (mirror
  is at-most-once per Python SDK contract). No batching yet — each
  frame appends on arrival; Python SDK's 100 ms batch cadence is still
  outstanding and tracked for the weekly parity check.
- **`OptionsBuilder::session_store_arc`** — alternate one-arg entry
  point that accepts `Arc<dyn SessionStore>` directly, so callers that
  want to keep a handle on the store (e.g. to inspect it after a
  client exits) don't need to double-wrap.

### Changed

- `Client` grows a `session_store: Option<Arc<dyn SessionStore>>`
  field captured at `spawn` time. Manual `Debug` impl shows `"<store>"`
  when set.

### Known

- Exact wire shape of `transcript_mirror` is a **best-guess** pending
  the 2026-04-27 weekly parity check — Python SDK source was not
  available in this session. Assumed shape (single frame per entry
  batch, key inlined on the frame):
  `{"type":"system","subtype":"transcript_mirror","session_id":"...","project_key":"...","subpath":null,"entries":[...]}`.
  Frames that don't match are logged and dropped; the event loop does
  not crash. Source comment on `Client::handle_transcript_mirror`
  flags this for verification.

## [0.1.1] — 2026-04-21

Deferred-item follow-up after v0.1.0 — closes the gaps listed in the
v0.1.0 "Known gaps" section.

### Added

- **`initialize` control_request (C2.9):** Sent automatically on
  `Client::spawn` after the system/init message. Carries the hook
  registry (event → matcher + hookCallbackIds + timeout), skills list
  (concrete names; `"all"` continues to travel via `--allowedTools`),
  `excludeDynamicSections` bool, and `agents` placeholder. Waits for a
  matching `control_response` before accepting user input. Protocol
  matches Python SDK v0.1.64 `_internal/query.py`.
- **Request-id generator** matching Python's `req_<counter>_<hex4>`
  shape (`request_id::next()`).
- **`permission_prompt_tool_name` option** — orthogonal permission
  path via CLI flag `--permission-prompt-tool <name>`.
- **`session_store` option + `--session-mirror` CLI flag** — when a
  `SessionStore` is attached, forge-sdk passes `--session-mirror` so
  the CLI emits `transcript_mirror` frames. (Frame ingestion into
  `store.append(...)` still to follow in a later patch — the wire
  plumbing is in place.)
- **CLI-version guard on spawn** — runs `<binary> --version` once and
  checks the reported major version meets `minimum_cli_version`
  (default `"2.0.0"`, matching Python SDK's pin at
  `subprocess_cli.py:29`). Pass `None` to disable.
- **8 outbound control subtypes** on `Client`:
  `interrupt`, `set_permission_mode`, `rewind_files`, `mcp_reconnect`,
  `mcp_toggle`, `stop_task`, `mcp_status`, `get_context_usage`.
- **`fork_session`** on `Client` — sends the `fork_session`
  control_request with an optional `tool_use_id` split-point and
  returns the new `session_id` the CLI assigned.
- **Mock fixtures:** `mock_claude_raw.sh` for transport-level tests
  that bypass `Client::spawn`; existing mocks updated to answer the
  initialize handshake.

### Changed

- `Options` grows `permission_prompt_tool_name`, `session_store`,
  `minimum_cli_version` fields. `Debug` impl updated.

### Known gaps still outstanding

- Transcript-mirror frame ingestion (`transcript_mirror` →
  `session_store.append`) — flag is passed but we don't yet parse the
  frames on the stdio stream.
- No integration tests for the 8 new control subtypes or
  `fork_session`; real-claude smoke coverage would need the live
  binary.

## [0.1.0] — 2026-04-21

First parity release against `claude-agent-sdk` v0.1.64.

### Added

- **M0 scaffolding:** Cargo workspace, nightly toolchain pin, clippy-pedantic
  lint config, rustfmt, cargo-deny, GitHub Actions CI across macOS + Linux.
- **M1 core transport:** `Client`, `OptionsBuilder`, `PermissionMode` (6
  variants including `DontAsk`), stream-json codec, `tokio::process`
  subprocess wrapper, mock-binary integration tests, real-`claude` smoke
  test, echo example.
- **M2 permissions:** `can_use_tool` callback, `PermissionDecision` with
  allow / allow-with-input / deny; `ToolPermissionContext` with
  `tool_use_id` + `agent_id`; control-protocol types (`ControlRequest`,
  `ControlResponse`) with Python-compatible wire keys (`updatedInput`
  camelCase, `interrupt` skip-when-false).
- **M3 in-process MCP:** `Tool` trait, `tool!` declarative macro,
  `McpServer` + `McpServerBuilder` with JSON-RPC `dispatch` (pure,
  transport-free); `McpHosts` routing; `--mcp-config` inline JSON with
  `type: "sdk"` signal; `mcp_message` control-request handling.
- **M4 hooks:** 10 hook kinds (`PreToolUse`, `PostToolUse`,
  `PostToolUseFailure`, `UserPromptSubmit`, `Stop`, `SubagentStop`,
  `SubagentStart`, `PreCompact`, `Notification`, `PermissionRequest`) +
  `HookKind::Unknown` fallback; `HooksBuilder` with matcher-based
  registration; `HookDecision` (allow / deny / replace-input /
  passthrough); dispatch by opaque `callback_id` registered at spawn;
  `hookSpecificOutput` wrapper emits per-event shape (`PreToolUse` →
  `updatedInput`, `UserPromptSubmit` → `updatedPrompt`).
- **M5 session store:** `SessionStore` trait with two required methods
  (`append`, `load`) and three optional (`list_sessions`, `delete`,
  `list_subkeys`) defaulting to `NotImplemented`;
  `SessionKey` + `SessionListSubkeysKey` + `SessionStoreEntry` +
  `SessionStoreListEntry` types matching Python wire shape;
  `MemorySessionStore` + `FsSessionStore` impls with cascading delete
  and subkey enumeration.
- **M6 recent additions:** `skills` option (three-channel delivery —
  `--allowedTools` Skill injection + `--setting-sources=user,project`
  default + `initialize` payload [deferred]), `allowed_tools` explicit
  option, `setting_sources` override, `exclude_dynamic_sections` toggle
  (wire shape in `initialize` `control_request`, not CLI flag), tracing
  bridge with `turn_span` / `tool_span` / `hook_span` helpers.
- **M7 polish:** CHANGELOG, `docs/parity-check.md` weekly runbook,
  `PARITY.md` upstream tracking, cargo-deny config, `cargo publish
  --dry-run` CI job.

### Known gaps vs. Python v0.1.64

- `initialize` control_request not yet sent. Hooks still work because
  dispatch is by `callback_id` (SDK mints locally at spawn) but the CLI
  doesn't yet know the registered set. `skills` and
  `exclude_dynamic_sections` likewise travel only as CLI flags /
  allowedTools injection until the initialize task lands.
- Additional control subtypes (`interrupt`, `set_permission_mode`,
  `rewind_files`, `mcp_reconnect`, `mcp_toggle`, `stop_task`,
  `mcp_status`, `get_context_usage`) not implemented. Parity tracked
  per upcoming weekly runs.
- `fork_session`, `permission_prompt_tool_name`, `--session-mirror`
  CLI wiring, CLI-version guard at spawn — deferred to v0.1.x
  follow-ups.

## [0.0.2] — 2026-04-21

- M2 permissions + M3 in-process MCP (see 0.1.0 entry for full details).

## [0.0.1] — 2026-04-21

- M0 scaffolding + M1 core transport (see 0.1.0 entry for full details).
