# Changelog

All notable changes to `forge-sdk` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Version numbers mirror the Python SDK release they target parity with
(e.g. `forge-sdk 0.1.0` targets `claude-agent-sdk` v0.1.64+).

## [Unreleased]

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
