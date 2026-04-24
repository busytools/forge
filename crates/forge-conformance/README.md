# forge-conformance

Wire-conformance harness for forge-sdk. Verifies that forge-sdk's wire
contract with the `claude` CLI is intact for a specific pinned CLI
version.

**Not a library consumers use.** Dev tooling only. Kept in a sibling
workspace crate so the forge-sdk crate stays focused on the SDK surface
library consumers actually use.

## The model

forge-sdk is **pinned to a specific `claude` CLI version at any time**
(see `PINNED_CLI_VERSION` in `src/lib.rs`). The harness is the contract
between forge-sdk and that CLI version. When Anthropic ships a new CLI
version, we run the upgrade ritual:

1. Run the full harness against the new CLI version.
2. Diff the captured traces against the committed baselines.
3. If green: bump the pinned version, recommit baselines, done.
4. If not: fix the divergences in forge-sdk, then bump.

## Two test modes per scenario

Every scenario has two halves:

### Replay mode (default, runs on every `cargo test` / `just check`)

Loads the committed baseline trace for the scenario from
`baselines/<pinned-version>/<scenario>.jsonl`, feeds every inbound line
through `forge_sdk::transport::codec::decode_dispatch`, asserts:

- Every line decodes.
- No `DecodedLine::Unknown` variants seen (unless explicitly expected).
- No `ControlRequestKind::Unknown` variants seen.

Runs offline. No API cost. Guards against decoder regressions in
forge-sdk itself.

### Live capture mode (opt-in via `FORGE_WIRE_CAPTURE=1`)

Spawns the real `claude` binary with the scenario's options, drives
forge-sdk through the prompt/response flow, records every stdin/stdout
line to `target/wire-traces/capture-<scenario>-<ts>.jsonl`, then:

1. Asserts the run succeeded end-to-end (no panics, Result frame seen).
2. Verifies every captured inbound line decodes cleanly.
3. If you want, the fresh capture can be promoted into the committed
   baseline with `cp target/wire-traces/capture-*.jsonl
   crates/forge-conformance/baselines/<version>/<scenario>.jsonl`.

Burns real API tokens. Runs only when you explicitly enable it. Used
to capture fresh baselines when the pinned CLI version bumps, or to
validate that forge-sdk is still wire-compatible against the live
binary.

## Running

```bash
# Replay mode — offline, always safe:
cargo nextest run -p forge-conformance

# Live capture — burns tokens:
FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-conformance \
  --no-capture --run-ignored all

# Single live scenario:
FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-conformance \
  --no-capture --run-ignored only wire_capture_trivial_prompt
```

## Adding a new scenario

1. Write a live-capture test in `tests/<scenario>.rs` following the
   pattern in `tests/wire_conformance.rs`:
   - `#[tokio::test] #[ignore]`, gated on `FORGE_WIRE_CAPTURE`.
   - Use `RecordingTransport::new(Subprocess::spawn(...)...)` +
     `Client::spawn_with_transport`.
   - Drive the scenario. Capture the trace to `target/wire-traces/`.
2. Run it with `FORGE_WIRE_CAPTURE=1` — this produces the capture.
3. Copy the capture into `baselines/<version>/<scenario>.jsonl`.
4. The always-on `replay.rs::all_baselines_decode_cleanly` test picks up
   the new baseline automatically.
5. Commit the test + baseline.

## Directory layout

```
crates/forge-conformance/
├── Cargo.toml                      # workspace member, not published
├── README.md                       # this file
├── src/
│   └── lib.rs                      # TraceLog, RecordingTransport, run_live_scenario,
│                                   # decode_all_inbound, baseline loader,
│                                   # PINNED_CLI_VERSION
├── baselines/
│   └── 2.1.117/                    # pinned CLI version at capture time
│       ├── trivial.jsonl           # one scenario per file (see list below)
│       ├── bash_tool.jsonl
│       ├── multi_turn.jsonl
│       ├── pretooluse_hook.jsonl
│       ├── mcp_status.jsonl
│       ├── in_process_mcp.jsonl
│       ├── context_usage.jsonl
│       └── set_permission_mode.jsonl
└── tests/
    ├── replay.rs                   # always-on decode test against every baseline
    ├── wire_conformance.rs         # trivial scenario
    ├── scenarios_bash_tool.rs      # Bash tool invocation
    ├── scenarios_multi_turn.rs     # two turns in one session
    ├── scenarios_pretooluse_hook.rs # PreToolUse hook callback
    ├── scenarios_mcp_status.rs     # outbound mcp_status
    ├── scenarios_in_process_mcp.rs # SDK-hosted MCP server + tool call
    ├── scenarios_context_usage.rs  # outbound get_context_usage
    ├── scenarios_permission_mode.rs # outbound set_permission_mode
    └── debug_smoke.rs              # raw-wire diagnostic (FORGE_WIRE_DEBUG=1)
```

## Coverage map (baselines vs. wire surfaces)

| Surface                                 | Scenario(s)                      |
|-----------------------------------------|----------------------------------|
| `system/init`                           | all                              |
| `system/hook_started` + `hook_response` | all (SessionStart fires)         |
| `assistant` (text content)              | all                              |
| `assistant` (`tool_use` content)        | bash_tool, pretooluse_hook, in_process_mcp |
| `user` (`tool_result` content)          | bash_tool, pretooluse_hook, in_process_mcp |
| `rate_limit_event`                      | all                              |
| `result`                                | all                              |
| `control_request: initialize` (out)     | all                              |
| `control_response: success` (in)        | all                              |
| `control_request: hook_callback` (in)   | pretooluse_hook                  |
| `control_request: mcp_message` (in)     | in_process_mcp                   |
| `control_request: mcp_status` (out)     | mcp_status                       |
| `control_request: get_context_usage` (out) | context_usage                 |
| `control_request: set_permission_mode` (out) | set_permission_mode        |
| Multi-turn / session continuity         | multi_turn                       |

Known gaps (not yet captured): `can_use_tool` control_request (blocked
by a forge-sdk wiring bug — see project auto-memory
`project_can_use_tool_wire_gap`), `interrupt` (needs mid-turn timing),
`task_*` messages (needs subagent scenario), `stream_event`,
`compacted` / `compact_error`, `control_cancel_request`.

## Upgrade ritual (when claude CLI bumps)

1. Update `PINNED_CLI_VERSION` in `src/lib.rs`.
2. Create `baselines/<new-version>/`.
3. Run every live-capture test once with `FORGE_WIRE_CAPTURE=1`.
4. For each scenario, diff the new capture against the old baseline.
5. Investigate any new frames / types / subtypes.
6. If forge-sdk needs changes to handle them, ship those first.
7. Commit new baselines + `PINNED_CLI_VERSION` bump + any fixes
   together.
8. Delete the old `baselines/<old-version>/` directory.
