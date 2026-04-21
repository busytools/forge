# Protocol notes — observed Python SDK behaviour

> Committed to the forge repo under `docs/`. **Not** the user-level planning docs.

Source of truth: `claude-agent-sdk` Python v0.1.64 (`_internal/query.py`,
`_internal/transport/subprocess_cli.py`, `types.py`). Findings consolidated
from the two parallel plan-reviews captured in
`~/.claude-stargate/plans/2026-04-21-forge-sdk-corrections.md`.

---

## Permission flow (`can_use_tool`)

### Mechanism

(a) stream-json control messages over the same stdio pipe as regular
messages. There is NO separate MCP tool for permission prompts in the
default flow. `permission_prompt_tool_name` exists as an orthogonal
opt-in mechanism (Options-level CLI flag `--permission-prompt-tool`),
but the `can_use_tool` callback path is purely control-message.

### Request shape (binary → SDK)

```json
{
  "type": "control_request",
  "request_id": "req_1_a1b2c3d4",
  "request": {
    "subtype": "can_use_tool",
    "tool_name": "Edit",
    "input": {"file_path": "/tmp/foo.rs"},
    "permission_suggestions": [],
    "blocked_path": null,
    "tool_use_id": "toolu_01XyZ",
    "agent_id": null
  }
}
```

Key fields:
- `tool_use_id` (required, string) — correlates with the subsequent
  `ToolResult` in the stream.
- `agent_id` (optional) — present when a Task-spawned sub-agent is
  making the call.
- `permission_suggestions` (array, may be empty) — hints from the CLI.
- `blocked_path` (optional) — workspace sandbox block indicator.

There is **no** `parent_tool_use_id` on the request (early planning
assumption was wrong).

### Response shape (SDK → binary)

**Allow** (always echoes `updatedInput`, even when the callback had no
override — the SDK writes the original `input` back):

```json
{
  "type": "control_response",
  "request_id": "req_1_a1b2c3d4",
  "response": {
    "subtype": "success",
    "response": {
      "behavior": "allow",
      "updatedInput": {"file_path": "/tmp/foo.rs"},
      "updatedPermissions": null
    }
  }
}
```

**Deny**:

```json
{
  "type": "control_response",
  "request_id": "req_1_a1b2c3d4",
  "response": {
    "subtype": "success",
    "response": {
      "behavior": "deny",
      "message": "not today",
      "interrupt": false
    }
  }
}
```

- `updatedInput` / `updatedPermissions` are **camelCase** on the wire.
- `interrupt` is a plain `bool` but only serialised when `true`
  (Python emits it with `skip_serializing_if = std::ops::Not::not`).

### Dispatch

Python routes the request to `options.can_use_tool` (an async callable).
Exceptions inside the callback default-deny with a "callback failed"
message; they are NOT re-raised into the main task.

### Input modification

Python supports `PermissionResultAllow(updated_input=...)`. When the
Rust API's `PermissionDecision::allow()` is used (no override), the
SDK layer MUST thread the original `input` from the request through
to the wire `updatedInput` — the field is never null on the allow
branch.

---

## Request ID format

Python generates IDs as `req_<counter>_<hex4>`:

```
req_0_e4f1a2b3
req_1_c3d4e5f6
```

(See `_internal/query.py`.) Rust side: use `getrandom::fill` +
`hex::encode` for the hex, `AtomicU64` for the counter.

---

## MCP in-process hosting

### Mechanism

MCP servers declared via `ClaudeAgentOptions.mcp_servers` with type
`"sdk"` are **in-process** — no child subprocess, no UNIX socket, no
bridge binary. The SDK owns the `mcp.server.Server` instance and
dispatches JSON-RPC messages directly.

### Config delivery

SDK passes `--mcp-config '<json>'` as **inline JSON on argv** (not a
temp file). For in-process servers, the JSON is:

```json
{
  "mcpServers": {
    "my_server": {"type": "sdk", "name": "my_server"}
  }
}
```

The `"type": "sdk"` signals to the CLI that the parent process hosts
this server.

### Wire transport

When the CLI wants to call an MCP tool, it emits:

```json
{
  "type": "control_request",
  "request_id": "req_5_...",
  "request": {
    "subtype": "mcp_message",
    "server_name": "my_server",
    "message": {
      "jsonrpc": "2.0",
      "id": 3,
      "method": "tools/call",
      "params": {"name": "greet", "arguments": {"name": "world"}}
    }
  }
}
```

The SDK dispatches in-process (via `McpServer::dispatch`) and
responds:

```json
{
  "type": "control_response",
  "request_id": "req_5_...",
  "response": {
    "subtype": "success",
    "response": {
      "mcp_response": {
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": "hello, world"}]}
      }
    }
  }
}
```

Key: the JSON-RPC body is wrapped as `{"mcp_response": <jsonrpc>}` inside
`control_response.response` (matches Python `query.py:413`).

For JSON-RPC **notifications** (no id), the SDK synthesises a wrapper
`{"jsonrpc":"2.0","result":{}}` so a `control_response` is always
emitted.

### allowed-tools

Python does NOT auto-inject `mcp__<server>__<tool>` into
`--allowedTools`. The caller sets their own `allowed_tools` list.
forge-sdk mirrors this — no auto-injection.

---

## Initialize control_request

First frame after connecting (SDK → CLI). Carries hook registry,
skills, excludeDynamicSections, and agent definitions — these are
NOT CLI flags despite earlier planning assumptions.

```json
{
  "type": "control_request",
  "request_id": "req_0_...",
  "request": {
    "subtype": "initialize",
    "hooks": {
      "PreToolUse": [
        {
          "matcher": "Bash",
          "hookCallbackIds": ["hook_0"],
          "timeout": 30
        }
      ]
    },
    "excludeDynamicSections": false,
    "skills": ["create-story"],
    "agents": {}
  }
}
```

- `hookCallbackIds` are SDK-minted opaque strings (`hook_0`, `hook_1`,
  ...). SDK keeps a `HashMap<String, Arc<dyn ErasedHookCallback>>`.
- `excludeDynamicSections` and `skills` are in this payload, not CLI
  flags.

The CLI replies with `{subtype: "success"}` or `{subtype: "error", ...}`.

---

## Hook callback flow

### Wire shape (CLI → SDK)

```json
{
  "type": "control_request",
  "request_id": "req_9_...",
  "request": {
    "subtype": "hook_callback",
    "callback_id": "hook_0",
    "tool_use_id": "toolu_01...",
    "input": {
      "hook_event_name": "PreToolUse",
      "tool_name": "Bash",
      "tool_input": {"command": "ls"},
      "session_id": "sess_01",
      "transcript_path": "/path/to/transcript.jsonl",
      "cwd": "/Users/u/project",
      "tool_use_id": "toolu_01..."
    }
  }
}
```

Dispatch by `callback_id`, NOT by `hook_name`. Event identity is
embedded in `input.hook_event_name` (for deserialisation).

### Hook kinds

10 hook kinds total (not 6 as earlier plans assumed):

- `PreToolUse`, `PostToolUse`, `PostToolUseFailure`
- `UserPromptSubmit`
- `Notification`, `SessionStart`, `SessionEnd`
- `SubagentStart`, `Stop`
- `PermissionRequest`

Every hook input also carries `BaseHookInput`: `session_id`,
`transcript_path`, `cwd`, `permission_mode`.

### Response shape

Per-event `hookSpecificOutput` wrapper:
- `PreToolUse` → `{hookEventName: "PreToolUse", updatedInput: ...}`
- `UserPromptSubmit` → `{hookEventName: "UserPromptSubmit", updatedPrompt: ...}`
- Others → warn + skip (Python matches
  `PreToolUseHookSpecificOutput` at `types.py:369-376`).

---

## SessionStore (transcript-mirror adapter)

SessionStore is NOT a state-store backend — the CLI always writes to
local disk. The SessionStore receives a secondary copy of each JSONL
line via the `--session-mirror` CLI flag.

```python
class SessionStore(Protocol):
    async def append(self, key: SessionKey, entries: list[SessionStoreEntry]) -> None: ...   # REQUIRED
    async def load(self, key: SessionKey) -> list[SessionStoreEntry] | None: ...              # REQUIRED
    async def list_sessions(self, project_key: str) -> list[SessionStoreListEntry]: ...       # OPTIONAL
    async def delete(self, key: SessionKey) -> None: ...                                      # OPTIONAL
    async def list_subkeys(self, key: SessionListSubkeysKey) -> list[str]: ...                # OPTIONAL
```

`delete` on a main-transcript key (no subpath) cascades to subkeys —
no separate `delete_cascading`.

At-most-once delivery; failed `append` batches surface as
`mirror_error` system messages and are NOT retried.

---

## Skills option (three-channel delivery)

When `options.skills` is non-empty, Python does THREE things:

1. Inject `Skill` (for `"all"`) or `Skill(<name>)` per skill into
   `--allowedTools`. `"all"` → bare `"Skill"` (no parens).
2. If `options.setting_sources` is unset, default to
   `["user", "project"]` and emit `--setting-sources=user,project`.
3. Send the `skills` field in the initialize control_request.

Only the concrete-list case populates the `skills` field in
initialize — `"all"` injects bare `"Skill"` in allowedTools only.

---

## CLI flag conventions

- `--allowedTools` (camelCase, NOT `--allowed-tools`).
- `--permission-mode <str>` (kebab-case).
- `--mcp-config '<json>'` (inline JSON).
- `--session-mirror` (boolean flag).
- `--permission-prompt-tool <name>` (optional orthogonal permission path).
- `--setting-sources <comma,list>`.

---

## References

- Python SDK source (v0.1.64): `~/.venv/lib/python3.14/site-packages/claude_agent_sdk/`
- Plan corrections consolidation: `~/.claude-stargate/plans/2026-04-21-forge-sdk-corrections.md`
- Live observations: deferred (SDK captures above drawn from review
  pass, not from re-running Python live in this session).
