# forge-test-harness

Wire-conformance harness for forge:

- **`sdk_wire`** — forge-sdk ↔ `claude` CLI stream-json

Ships scenarios + committed baselines + a replay test that runs on
every `cargo nextest run`. Live capture is opt-in via
`FORGE_WIRE_CAPTURE=1`.

**Not a library consumers use.** Dev tooling only.

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
   crates/forge-test-harness/baselines/sdk/<version>/<scenario>.jsonl`.

Burns real API tokens. Runs only when you explicitly enable it. Used
to capture fresh baselines when the pinned CLI version bumps, or to
validate that forge-sdk is still wire-compatible against the live
binary.

## Running

```bash
# Replay mode — offline, always safe:
cargo nextest run -p forge-test-harness

# Live capture — burns tokens:
FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
  --no-capture --run-ignored all

# Single live scenario:
FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
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
crates/forge-test-harness/
├── Cargo.toml                          # workspace member, not published
├── README.md                           # this file
├── src/
│   └── lib.rs                          # TraceLog, RecordingTransport, run_live_scenario,
│                                       # decode_all_inbound, baseline loader,
│                                       # PINNED_CLI_VERSION
├── baselines/
│   └── 2.1.117/                        # pinned CLI version at capture time
│       └── <scenario>.jsonl × 23       # one baseline per scenario (see table below)
└── tests/
    ├── replay.rs                       # always-on decode test across every baseline
    ├── wire_conformance.rs             # trivial smoke
    ├── scenarios_bash_tool.rs
    ├── scenarios_multi_turn.rs
    ├── scenarios_pretooluse_hook.rs
    ├── scenarios_mcp_status.rs
    ├── scenarios_in_process_mcp.rs
    ├── scenarios_context_usage.rs
    ├── scenarios_permission_mode.rs
    ├── scenarios_subagent.rs           # Task tool + task_* lifecycle frames
    ├── scenarios_hook_events.rs        # user_prompt_submit, post_tool_use,
    │                                   # post_tool_use_failure, stop,
    │                                   # subagent_stop, notification
    ├── scenarios_outbound_controls.rs  # set_model, mcp_reconnect, mcp_toggle,
    │                                   # rewind_files, stop_task, interrupt
    ├── scenarios_compact.rs            # /compact slash command + PreCompact hook
    ├── scenarios_stream_event.rs       # include_partial_messages → stream_event
    └── debug_smoke.rs                  # raw-wire diagnostic (FORGE_WIRE_DEBUG=1)
```

## Real-session probe

The harness also ships an opt-in decoder probe against Claude Code's
on-disk session files
(`$CLAUDE_CONFIG_DIR/projects/<slug>/<session-id>.jsonl`). These use
a persistence-format superset of stream-json; the
`session_redact::transform_persistence_line` helper rewrites each line
into wire shape + scrubs PII, then the probe feeds everything through
the decoder.

Run it against all of the developer's recorded sessions:

```bash
FORGE_REAL_SESSIONS=$HOME/.claude-stargate/projects \
  cargo nextest run -p forge-test-harness --no-capture \
  real_session_decode_probe
```

Nothing is persisted — failures surface as stderr lines and a panic.
This is how the `document` content-block gap that forced the
`ContentBlock::Unknown` forward-compat variant was surfaced.

To produce a redacted, **committed** baseline from a specific
session:

```bash
cargo run -p forge-test-harness --example redact_session -- \
  $HOME/.claude-stargate/projects/<slug>/<session>.jsonl \
  crates/forge-test-harness/baselines/sdk/2.1.117/real_session_<name>.jsonl
```

One sample lives at `baselines/2.1.117/real_session_sample.jsonl`
— 352 messages covering real multi-turn tool-use flows, all
redaction-scrubbed.

## Coverage map (wire surfaces vs. scenarios)

| Wire surface                                | Captured by                                 |
|---------------------------------------------|---------------------------------------------|
| `system/init`                               | all                                         |
| `system/hook_started` + `hook_response`     | all (SessionStart fires from user profile)  |
| `system:status`                             | compact, stream_event                       |
| `system:compact_boundary`                   | compact                                     |
| `system:task_started` / `task_progress` / `task_notification` / `task_updated` | subagent, stop_task, subagent_stop_hook |
| `assistant` (text)                          | all                                         |
| `assistant` (`tool_use`)                    | bash_tool, pretooluse_hook, post_tool_use_hook, post_tool_use_failure_hook, in_process_mcp, subagent, stop_task |
| `assistant` (`thinking`)                    | subagent                                    |
| `user` (text)                               | all                                         |
| `user` (`tool_result`)                      | bash_tool, pretooluse_hook, in_process_mcp  |
| `rate_limit_event`                          | all                                         |
| `stream_event`                              | stream_event                                |
| `result: success`                           | all non-error scenarios                     |
| `result: error_during_execution`            | interrupt                                   |
| `control_request: initialize` (out)         | all                                         |
| `control_request: hook_callback` (in)       | every hook scenario                         |
| `control_request: mcp_message` (in)         | in_process_mcp                              |
| `control_request: mcp_status` (out)         | mcp_status, mcp_reconnect, mcp_toggle       |
| `control_request: get_context_usage` (out)  | context_usage                               |
| `control_request: set_permission_mode` (out)| set_permission_mode                         |
| `control_request: set_model` (out)          | set_model                                   |
| `control_request: mcp_reconnect` (out)      | mcp_reconnect                               |
| `control_request: mcp_toggle` (out)         | mcp_toggle                                  |
| `control_request: rewind_files` (out)       | rewind_files                                |
| `control_request: stop_task` (out)          | stop_task                                   |
| `control_request: interrupt` (out)          | interrupt                                   |
| `control_request: can_use_tool` (in)        | permission_deny                             |
| `control_cancel_request` (in)               | control_cancel                              |
| `control_response: success` (in)            | all                                         |
| `control_response: deny` (out, `behavior`)  | permission_deny                             |
| Multi-turn / session continuity             | multi_turn                                  |

**Remaining gaps** (genuinely unreachable without custom CLI behaviour):

| Gap                               | Reason                                      |
|-----------------------------------|---------------------------------------------|
| `error` message variant           | CLI emits this only on fatal transport failures that are hard to simulate in a healthy session |

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
