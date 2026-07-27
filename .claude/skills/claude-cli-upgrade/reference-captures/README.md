# Reference captures — wire shapes for new CLI 2.1.156 tools

This directory holds raw `stream-json` captures from `claude --print` probes that nudged the model to invoke each new tool family. Use these when designing forge UI surfaces (Inspector pane rendering, chat suppression rules, glyph mapping, special-case routing) so the implementation works against real wire data rather than guesses.

Captured against `claude` CLI **2.1.156** on **2026-05-29**. If a newer CLI ships a different tool surface, regenerate via the `claude-cli-upgrade` skill (`Phase 4c`).

## How to read a capture

Each file is a JSONL stream — one JSON object per line — straight from `claude --print --output-format stream-json --verbose`. The interesting frames are:

- `{"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "X", "input": {...}}]}}` — the model invoking a tool. The `input` field is the wire shape forge-tui needs to handle.
- `{"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "...", "content": "..."}]}}` — the CLI's tool result. Forge-tui renders this back into the chat / inspector.
- `{"type": "system", "subtype": "<name>", ...}` — system events. Notable new one in 2.1.156: `thinking_tokens` (carries `estimated_tokens` + `estimated_tokens_delta`).

To extract just the tool calls and results from a capture:

```bash
python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    for line in f:
        try: obj = json.loads(line)
        except: continue
        msg = obj.get('message', {})
        if not isinstance(msg, dict): continue
        for block in msg.get('content', []) or []:
            if isinstance(block, dict) and block.get('type') in ('tool_use', 'tool_result'):
                print(json.dumps(block, indent=2))
" <capture-file.jsonl>
```

## Captures

| File | Tool family | What it covers |
|------|-------------|----------------|
| `task_create.jsonl` | TaskCreate | Single tool call creating one task with subject + description; result text carries the assigned `Task #N` integer ID |
| `task_update.jsonl` | TaskUpdate | TaskCreate → TaskUpdate(in_progress) → TaskUpdate(completed) → TaskList. Shows id-keyed status transitions and the listing shape |
| `task_list.jsonl` | TaskList | TaskList with no args — returns `{id, subject, status, owner, blockedBy[]}` array shape |
| `task_get.jsonl` | TaskGet | TaskCreate → TaskGet on the new ID. Shows the per-task detail wire shape |
| `schedule_wakeup.jsonl` | ScheduleWakeup | Single ScheduleWakeup call with a delay + reason. Shows the input parameters and result envelope |
| `tool_search.jsonl` | ToolSearch | Single ToolSearch with a string query. Used widely by the model to look up tools it doesn't yet have schemas for |
| `skill.jsonl` | Skill | Skill invocation tool call. Shows how the model invokes a named skill |
| `enter_exit_plan_mode.jsonl` | EnterPlanMode / ExitPlanMode | Plan-mode entry + exit pair. Shows both wire shapes back to back |
| `workflow.jsonl` | Workflow | Workflow tool invocation — also includes a TaskOutput call the model made transitively |
| `cron_lifecycle.jsonl` | CronCreate / CronList / CronDelete | Cron entry create → list → delete cycle, all three wire shapes in one capture |
| `lsp.jsonl` | LSP | LSP tool invocation against a Rust file in the working dir |
| `remote_trigger.jsonl` | RemoteTrigger | RemoteTrigger call with a plausible target. Model called ToolSearch first then RemoteTrigger once |
| `push_notification.jsonl` | PushNotification | PushNotification with title + body. Model called ToolSearch first then PushNotification once |

### Intentionally skipped

- **`Monitor`** — the Monitor tool is inherently streaming/long-lived (it watches a file or process and emits events). It has no natural exit condition for a single-shot `claude --print` probe — the model gets stuck in the watch loop and the capture never terminates. To capture Monitor's wire shape, either: (a) write a proper scenario in `crates/forge-test-harness/tests/sdk_scenarios_monitor.rs` that drives the harness to send an explicit stop event, or (b) capture from a real forge session that uses Monitor and exits cleanly. Skipping in the upgrade probe set is intentional, not a bug.

Captures from the existing live-test scenarios in `crates/forge-test-harness/baselines/sdk/2.1.156/` cover the rest of the wire surface (Bash, Read, Write, Edit, AskUserQuestion, Task subagent, MCP, hooks, permissions, workers, worktree, compact, control flows). Use those as reference for tools that already have forge-tui rendering.

## Integration checklist for forge UI work

For each new tool family above, before writing renderer code:

1. Inspect the capture file's `tool_use.input` shape — that's the data forge-tui receives.
2. Inspect the corresponding `tool_result.content` shape — that's what the CLI returns and forge-tui can render or summarize.
3. Decide: render as a regular tool-call card in chat (default), suppress from chat and route to a dedicated surface (like TodoWrite → Inspector TASKS), or both.
4. Add the tool name to `crates/forge-tui/src/ui/theme.rs` glyph table.
5. If the tool needs special routing, extend the gate in `crates/forge-tui/src/app/events/tool_calls.rs` and add a reducer in `crates/forge-tui/src/app/`.
6. Tests live in the existing wire-conformance scenario pattern under `crates/forge-test-harness/tests/sdk_scenarios_*.rs`. Adding one per tool family is ideal but additive — not blocking.

## Skill recap

To regenerate these captures against a future CLI version, re-run the `claude-cli-upgrade` skill's Phase 4c with whatever the current `claude` binary is. The skill captures the same set against the new wire and updates this directory.
