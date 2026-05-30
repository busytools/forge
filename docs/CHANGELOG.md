# Changelog

All notable changes to `forge-sdk` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Version numbers mirror the Python SDK release they target parity with
(e.g. `forge-sdk 0.1.0` targets `claude-agent-sdk` v0.1.64+).

## [Unreleased]

### Added  -  parity-gap closures (seventh pass, 2026-04-22)

- **`query_stream()`** returning `impl Stream<Item = Result<Message>>`
  alongside the Vec-collecting `query()`. Mirrors Python's
  `query() -> AsyncIterator[Message]` return shape. Driven by a
  spawned task + mpsc channel; consumer-drop tears down cleanly.
  Adds `tokio-stream = "0.1"` to workspace deps.
- **`SessionSummaryEntry`** + **`fold_session_summary()`** +
  **`summary_entry_to_sdk_info()`**  -  pure-fn port of Python's
  `_internal/session_summary.py`. Stores call `fold_session_summary`
  from inside `append()` to maintain per-session summary sidecars
  without re-reading the transcript. Lives in `src/session/summary.rs`.
- **`SessionStore::list_session_summaries()`**  -  optional trait
  method (default `NotImplemented`). Stores that maintain sidecars
  override to expose the `list_sessions_from_store()` fast path.
- **`pub trait Transport`** + **`Client::spawn_with_transport()`**  - 
  carves an extensibility seam out of the concrete `Subprocess`.
  Callers can now inject in-memory mocks, remote-SSH transports,
  containerised spawn, etc. Mirrors Python's abstract `Transport`
  base.
- **`forge_sdk::testing::run_session_store_conformance()`**  -  327-line
  testing harness ported from Python's
  `claude_agent_sdk.testing.session_store_conformance`. Third-party
  `SessionStore` adapters call this to certify the 14 behavioural
  contracts. Auto-probes optional methods via `NotImplemented`
  detection; caller can also name methods to skip explicitly.

### Changed  -  breaking (pre-release)

- **`Client::sub`** internal field type changed from concrete
  `Subprocess` to `Box<dyn Transport>`. Public API callers
  unaffected; direct-access-to-sub callers (if any) must migrate.
- **`Subprocess::shutdown(self)`** is now an alias for
  `close(&mut self)`. The `&mut self` form is preferred; the
  consuming form stays for backward compatibility.
- **`MemorySessionStore::list_subkeys`** no longer applies
  `sanitise()` to subpath strings. The conformance contract requires
  verbatim round-trip  -  the prior sanitisation swapped `/` for `-`
  in returned subpaths. Filesystem-backed stores (`FsSessionStore`)
  still sanitise for on-disk layout as expected.

### Added  -  Tier 6 test-mirror completion (sixth pass, 2026-04-22)

Every in-scope Python test file (14/14) now has a named Rust
counterpart in `tests/python_parity/`:

- `types.rs` (45/45) · `rate_limit.rs` (5/5) ·
  `subprocess_buffering.rs` (6/10 + 4 gap-docs) · `client.rs` (3/6
  + 3 N/A) · `integration.rs` (5/5) · `mcp_large_output.rs` (12/15
  + 3 blocked) · `query.rs` (5/17 + 12 N/A) · `transport.rs` (32/73
  + 10 N/A) · `sessions.rs` (97/99 + 2 gaps) ·
  `session_mutations.rs` (52/52) · `session_resume.rs` (7/38 + 31
  N/A) · `session_helpers_store.rs` (41/41) ·
  `transcript_mirror.rs` (33/33) · `tool_callbacks.rs` (18/18) ·
  `sdk_mcp_integration.rs` (16/48 + 32 N/A) ·
  `streaming_client.rs` (21/31 + 10 N/A).

Out of scope (Python ecosystem-specific example stores):
`test_example_redis_*`, `test_example_s3_*`, `test_example_postgres_*`.

### Added  -  earlier 2026-04-22 rounds

- Third pass: `ServerToolUse` + `ServerToolResult` content blocks
  (advisor / web_search / web_fetch / code_execution family).
  `test_message_parser.py` completed at 45/45 (tier 6).
- Fourth pass: `project_key_for_directory` gap closures  -  NFC
  normalisation on the canonicalised path + `Option<&str>` accepting
  `None` to default to cwd. Adds `unicode-normalization` dep.
  Breaking: signature changed from `&str` to `Option<&str>`.
- Fifth pass: `validate_session_store_options` extracted as a pure
  function at `session::validation::`. `Client::spawn` now routes
  through it; the 6 Python `TestSessionStoreOptionsValidation` cases
  are ported.

## [0.1.64]  -  2026-04-21

Deep-dive parity pass against Python `claude-agent-sdk` v0.1.64.
Version number jumps from 0.1.3 to 0.1.64 so forge-sdk releases
mirror the Python SDK version they target parity with (per the
convention documented in `PARITY.md`).

### Fixed  -  wire-level divergences from Python SDK

- **`stop_task` payload.** Python `types.py:1519` requires
  `{"subtype":"stop_task","task_id":"..."}`. forge-sdk was sending
  `tool_use_id`  -  wrong field name AND wrong semantic (Python's
  `task_id` is a sub-agent task id, not a tool-use id). Signature
  changed: `Client::stop_task(task_id: &str)`.
- **`rewind_files` payload.** Python `types.py:1497` requires
  `user_message_id: str`. forge-sdk was sending an empty object, so
  the CLI had no message to rewind to. Signature changed:
  `Client::rewind_files(user_message_id: &str)`.
- **`mcp_reconnect` / `mcp_toggle` field name.** Python
  `types.py:1505, 1513` uses camelCase `serverName`. forge-sdk was
  sending snake_case `server_name`. Wire now matches; public
  `Client` method argument name is unchanged (`server_name`)  -  only
  the JSON field key differs.
- **CLI argv: `--input-format`.** Python
  `subprocess_cli.py:207` does not emit this flag. forge-sdk was
  emitting `--input-format stream-json`. Removed so argv matches
  byte-for-byte.
- **CLI argv: always-on `--permission-mode`.** Python only emits
  `--permission-mode` when the caller set one explicitly. forge-sdk
  always emitted `--permission-mode default`. Now conditional on
  `permission_mode != PermissionMode::Default`, so CLI falls back to
  its own default (and any user-level override).

### Changed

- Workspace version `0.1.3` → `0.1.64`. Future forge-sdk releases
  track the Python SDK version they mirror (e.g. v0.1.65 when
  Anthropic ships v0.1.65). See `PARITY.md` "Versioning convention".
- `html_root_url` bumped accordingly.

### Added

- Exhaustive deep-dive audit log in `PARITY.md` enumerating every
  public type / option / argv flag divergence against Python v0.1.64.

### Known gaps (tracked in PARITY.md for the 2026-04-27 weekly)

Broad surface still to port. See `PARITY.md` for the full list and
priority order. Highlights:

- `BaseHookInput` flattening (session_id, transcript_path, cwd,
  permission_mode) on every hook input type.
- Missing hook input types: `PermissionRequestHookInput`,
  `NotificationHookInput`, `SubagentStartHookInput`,
  `PostToolUseFailureHookInput`.
- Typed `hookSpecificOutput` wrappers per event.
- Full `TranscriptMirrorBatcher` (500-entry / 1-MiB eager flush + on_error
  → `MirrorErrorMessage`).
- `control_cancel_request` inbound handling.
- `AgentDefinition` + `agents` option.
- `RateLimitEvent` / `RateLimitInfo` system frames.
- Task lifecycle frames (`TaskStartedMessage`, `TaskProgressMessage`,
  `TaskNotificationMessage`, `TaskUsage`, `TaskBudget`).
- Offline session helpers (`list_sessions`, etc.) and
  `*_via_store` / `*_from_store` async variants.
- `McpServerConfig` variants (stdio/sse/http)  -  forge-sdk only models
  in-process.
- `PermissionUpdate` on `PermissionDecision::allow_with_input`.
- Options: `tools`, `disallowed_tools`, `system_prompt`,
  `continue_conversation`, `session_id`, `max_turns`,
  `max_budget_usd`, `fallback_model`, `betas`, `settings`, `add_dirs`,
  `env`, `extra_args`, `include_partial_messages`, `fork_session`
  (spawn flag), `plugins`, `thinking`, `effort`, `output_format`,
  `enable_file_checkpointing`, `task_budget`, `sandbox`,
  `max_buffer_size`, `stderr`-callback, `user`.

## [0.1.3]  -  2026-04-21

First weekly parity run (pulled ahead of 2026-04-27 cadence). Corrects
divergences from the actual Python SDK v0.1.64 wire protocol that were
shipped as "best-guess" in v0.1.2.

### Fixed

- **`transcript_mirror` wire shape.** Python SDK v0.1.64 emits
  `{"type":"transcript_mirror","filePath":...,"entries":[...]}` at the
  top level (`_internal/query.py:282-289`,
  `_internal/transcript_mirror_batcher.py:3`). v0.1.2 mistakenly
  assumed `{"type":"system","subtype":"transcript_mirror",...}` with
  inline `session_id`/`project_key`/`subpath`. The codec now parses
  the real shape into a new `DecodedLine::TranscriptMirror {file_path,
  entries}` variant; Client derives the `SessionKey` via
  `session_store::file_path_to_session_key()`.
- **`SessionStoreListEntry.mtime`** field name  -  Python wire key is
  `mtime` (not `mtime_ms`) per `types.py:1153-1159`. Public breaking
  change on the list-sessions return type; callers reading the field
  need a rename.
- **`project_key` sanitisation.** Now `[^a-zA-Z0-9]` → `-` with a
  djb2 hash suffix for inputs >200 chars, matching
  `_internal/sessions.py::_sanitize_path`. v0.1.2 used
  `[^a-zA-Z0-9_-]` → `_` (no hash fallback), which produced different
  directory names for the same project path.

### Added

- `session_store::file_path_to_session_key(file_path, projects_dir)`  - 
  derives a `SessionKey` from the `filePath` in a mirror frame.
  Mirrors Python `_internal/session_store.py:108-153`.
- `Options.projects_dir` + `OptionsBuilder::projects_dir()`  -  override
  the projects directory used to resolve mirror frame paths. Defaults
  to `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`.

### Changed

- `Client::next_event` routes `DecodedLine::TranscriptMirror` to a new
  internal handler and calls a (currently no-op) `flush_mirror()` on
  `Message::Result` arrival. Ready for mechanical swap to a real
  batcher.

### Still outstanding (tracked for next parity run)

- Full `TranscriptMirrorBatcher` (per-filePath coalescing, 500-entry /
  1-MiB eager flush, 60 s append timeout, `on_error` callback).
  Minimum-viable per-frame append is in place; flush-on-result is
  wired but currently empty.
- Offline session helpers (`list_sessions`, `get_session_info`,
  `get_session_messages`, `list_subagents`,
  `get_subagent_messages`, and the `_from_store` async variants). Not
  exposed yet.
- `control_cancel_request` inbound handling.
- Start `tests/python_parity/` Rust mirror of upstream tests.

## [0.1.2]  -  2026-04-21

Closes the two remaining gaps called out under the v0.1.1 "Known gaps
still outstanding" section.

### Added

- **Integration tests for the 9 outbound control subtypes**  - 
  `interrupt`, `set_permission_mode`, `rewind_files`, `mcp_reconnect`,
  `mcp_toggle`, `stop_task`, `mcp_status`, `get_context_usage`,
  `fork_session`. New `mock_claude_control.sh` fixture handles the
  initialize handshake, then loops parsing each outbound
  `control_request` and emitting a matching `control_response` with a
  canned payload per subtype. Tests assert the round-trip returns the
  decoded payload for `mcp_status` / `get_context_usage` /
  `fork_session`.
- **`transcript_mirror` frame ingestion**  -  `Client::next_event` now
  intercepts `system/transcript_mirror` frames emitted by the CLI under
  `--session-mirror`, parses the `SessionKey` + `entries` payload, and
  calls `session_store.append(&key, &entries).await`. Frames are
  swallowed (never surface via `next_event`); parse failures and
  append errors are logged at `warn!` and continue the event loop (mirror
  is at-most-once per Python SDK contract). No batching yet  -  each
  frame appends on arrival; Python SDK's 100 ms batch cadence is still
  outstanding and tracked for the weekly parity check.
- **`OptionsBuilder::session_store_arc`**  -  alternate one-arg entry
  point that accepts `Arc<dyn SessionStore>` directly, so callers that
  want to keep a handle on the store (e.g. to inspect it after a
  client exits) don't need to double-wrap.

### Changed

- `Client` grows a `session_store: Option<Arc<dyn SessionStore>>`
  field captured at `spawn` time. Manual `Debug` impl shows `"<store>"`
  when set.

### Known

- Exact wire shape of `transcript_mirror` is a **best-guess** pending
  the 2026-04-27 weekly parity check  -  Python SDK source was not
  available in this session. Assumed shape (single frame per entry
  batch, key inlined on the frame):
  `{"type":"system","subtype":"transcript_mirror","session_id":"...","project_key":"...","subpath":null,"entries":[...]}`.
  Frames that don't match are logged and dropped; the event loop does
  not crash. Source comment on `Client::handle_transcript_mirror`
  flags this for verification.

## [0.1.1]  -  2026-04-21

Deferred-item follow-up after v0.1.0  -  closes the gaps listed in the
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
- **`permission_prompt_tool_name` option**  -  orthogonal permission
  path via CLI flag `--permission-prompt-tool <name>`.
- **`session_store` option + `--session-mirror` CLI flag**  -  when a
  `SessionStore` is attached, forge-sdk passes `--session-mirror` so
  the CLI emits `transcript_mirror` frames. (Frame ingestion into
  `store.append(...)` still to follow in a later patch  -  the wire
  plumbing is in place.)
- **CLI-version guard on spawn**  -  runs `<binary> --version` once and
  checks the reported major version meets `minimum_cli_version`
  (default `"2.0.0"`, matching Python SDK's pin at
  `subprocess_cli.py:29`). Pass `None` to disable.
- **8 outbound control subtypes** on `Client`:
  `interrupt`, `set_permission_mode`, `rewind_files`, `mcp_reconnect`,
  `mcp_toggle`, `stop_task`, `mcp_status`, `get_context_usage`.
- **`fork_session`** on `Client`  -  sends the `fork_session`
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
  `session_store.append`)  -  flag is passed but we don't yet parse the
  frames on the stdio stream.
- No integration tests for the 8 new control subtypes or
  `fork_session`; real-claude smoke coverage would need the live
  binary.

## [0.1.0]  -  2026-04-21

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
- **M6 recent additions:** `skills` option (three-channel delivery  - 
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
  CLI wiring, CLI-version guard at spawn  -  deferred to v0.1.x
  follow-ups.

## [0.0.2]  -  2026-04-21

- M2 permissions + M3 in-process MCP (see 0.1.0 entry for full details).

## [0.0.1]  -  2026-04-21

- M0 scaffolding + M1 core transport (see 0.1.0 entry for full details).
