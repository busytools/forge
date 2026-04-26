# Audit Report — 2026-04-26

## Scope
Entire forge codebase — 140 Rust source files / 35,379 LOC across 5 crates (forge-sdk, forged, forge-tui, forge-conformance, forged-conformance).

## Threat Model & Scope Context

forge / forged / forge-tui is built for **personal use only**. The user runs forged on multiple machines (Mac Studio, Mac Mini, MacBook Pro) and connects from each as a "client" — every connection is the same human. There is no adversarial multi-tenant threat model. The documented WireGuard production posture is a mesh between the user's own machines = still trusted.

**This audit was re-scoped 2026-04-26 against this threat model.** Findings whose severity depended on an adversarial assumption (cross-client authorization, path traversal, info leak via verbose error strings, takeover-DoS) are removed. Findings that affect operational reliability *even in a single-user deployment* — unbounded queues that can OOM the daemon when a laptop sleeps mid-session, silent failures that hide diagnostic signal, concurrency bugs that corrupt transcripts — are kept and, where appropriate, demoted from "critical security" to "important reliability."

If the scope changes (sharing forged with a collaborator, public-interface deployment, multi-user, open-sourcing), surface to the user before re-running this audit. See `project_trust_model.md` in project auto-memory.

## Specialists Deployed
- dead-code-analyzer
- architecture-reviewer
- bug-hunter
- silent-failure-hunter
- code-simplifier
- api-reviewer

## Summary
0 critical · 18 important · 25 minor

## Start Here

1. **[important] crates/forged/src/methods/session.rs:484-582** — Session actor cancels `handle_control` mid-write — potential CLI deadlock
   The actor's `tokio::select!` cancels `client.next_event()` when a command arrives. `next_event` internally awaits `handle_control` which writes a `control_response`. If cancellation fires mid-write, the response is lost and the CLI hangs forever. This contradicts the documented rationale of the actor pattern in `project_forged_actor_pattern.md` — worth verifying with a targeted reproducer before fixing.

2. **[important] crates/forge-sdk/src/client/control_send.rs:131-135** — `send_control` silently drops regular messages mid-control-flight
   Inside the response-wait loop, regular assistant/user/system/result/stream_event frames are logged via `tracing::warn!` and dropped instead of being buffered like `spawn_inner` does. Mid-session `control_request`s (interrupt, set_permission_mode, etc.) cause transcript corruption when the CLI emits regular frames during the wait. Breaks parity with Python SDK.

3. **[important] crates/forge-tui/src/input.rs:49-52** — TUI `Esc` on permission modal orphans the prompt
   Pressing `Esc` clears focus but does NOT clear `pending_permission` and does NOT send a deny response. Reverse-RPC remains pending until the 1-hour `HOOK_TIMEOUT_SECS`. The user has no way to re-open the modal and cannot answer subsequent prompts — looks like a hang.

4. **[important] crates/forged/src/connection.rs:25, server.rs:94, registry.rs:114** — Unbounded mpsc channels (reliability, not security)
   Per-connection outbound and per-session command channels use `unbounded_channel`. A laptop sleeping mid-session blocks its WS receiver indefinitely; notifications, reverse-RPC requests, and broadcasts queue without limit. The Mac Studio daemon OOMs and kills every other session the user has running. Single-user trust does not save you from this — it's a backpressure problem.

5. **[important] crates/forged/src/methods/session.rs** (god file 1053 LoC) — Four responsibilities + duplicated takeover sequences + 12-site actor-call template
   Architecture finding (high confidence) plus two simplification findings (high/medium confidence) cluster on the same file: wire handlers + actor body + Options wire-mirror + parse_spawn_params live in one 1053-LoC file with two `#[allow(clippy::too_many_lines)]` smells. `subscribe()` and `claim_primary()` each carry near-identical 50+ LoC primary-takeover sequences (drift evidence: round 4 fix M3 had to be applied in both). 12 actor-routed handlers across session.rs/mcp.rs/context.rs match the same 8-line template.

## Important Findings

### crates/forge-sdk/src/client/control_send.rs:131-135
**send_control silently drops regular messages mid-control-flight**
- **Found by:** bug-hunter (high confidence)
- **Description:** Inside `send_control`'s response-wait loop, when an inbound line is neither `control_response` nor `control_request` — e.g. a regular assistant/user/system/result/stream_event/rate_limit_event frame — the code only logs `tracing::warn!` and drops it. Compare with `spawn_inner` (client.rs:322-352) which buffers regular messages into `pre_init_messages` for later replay.
- **Impact:** Lost messages whenever a mid-session control_request (interrupt, set_permission_mode, mcp_status, get_context_usage, set_model, rewind_files, mcp_reconnect, mcp_toggle, stop_task) is in flight while the CLI emits regular frames. With `--include-partial-messages` enabled, partial messages disappear from the conversation transcript entirely. Breaks parity with Python SDK and corrupts transcripts the daemon forwards.
- **Suggested fix:** When `decode_dispatch(...)` returns `DecodedLine::Message`, push into `self.pre_init_messages` (rename to `buffered_messages`) so `next_event` will surface it. Mirror handling for `DecodedLine::Unknown`.

### crates/forge-sdk/src/client.rs:438-441
**system/init filtering only applies pre-init**
- **Found by:** bug-hunter (medium confidence)
- **Description:** `next_event` drains `pre_init_messages` one at a time. The init filter at line 383-391 only removes `subtype == "init"` for pre-init buffered messages. If the CLI emits `system/init` post-init (session-resumed flows), `next_event` returns it untouched — contradicting the doc comment "drop the system/init frame so callers see clean post-init stream."
- **Impact:** Subscribers see system/init messages on resume/reconnect they don't expect. Drift from Python SDK behaviour.
- **Suggested fix:** Either centralise the filter in `next_event` so subsequent inits are also dropped, or document that the contract only holds for pre-init drain.

### crates/forge-sdk/src/transport/process.rs:292-294
**JoinError swallowed in Subprocess::close**
- **Found by:** silent-failure-hunter (medium confidence)
- **Description:** `let _ = task.await;` discards the JoinError from the stderr drain task. If the drainer panicked, the panic is hidden. Tokio prints panics to stderr by default, but the daemon's tracing setup doesn't connect those. Companion to bridged_transport.rs stderr handling below — same root in subprocess stderr lifecycle, separate files and separate fixes.
- **Impact:** Drain-task panics are silently lost.
- **Suggested fix:** `if let Err(e) = task.await { warn!(error = %e, "stderr drain task ended abnormally"); }`.

### crates/forge-sdk/src/session/mutations.rs:184
**Session fork pass-1 swallows serde_json::from_str errors**
- **Found by:** silent-failure-hunter (medium confidence)
- **Description:** `if let Ok(value) = serde_json::from_str::<Value>(line) { ... }` with no `else` branch. Malformed JSONL line is silently skipped in pass 1; the omission cascades into pass 2 producing potentially-broken parent-uuid references in forked output.
- **Suggested fix:** Add `else { tracing::debug!(error = %e, line_no = idx, "session fork: skipping unparseable line"); continue; }`.

### crates/forge-sdk/src/session/scan.rs:201,205
**Fork pass-1 lossy line skips can corrupt output silently**
- **Found by:** silent-failure-hunter (medium confidence)
- **Description:** `BufReader::new(reader).lines().map_while(Result::ok)` cleanly stops on first I/O error — remainder is skipped silently. Combined with `let Ok(value) = serde_json::from_str(&line) else { continue; };`, partial read + corrupted line truncates message-list output without warning.
- **Suggested fix:** Replace `map_while(Result::ok)` with an explicit loop that warns on Err before breaking.

### crates/forge-tui/src/client/connection.rs:233
**Duplicate session subscription overwrites silently**
- **Found by:** bug-hunter (medium confidence)
- **Description:** `subscribe_session` calls `self.subscriptions.lock().insert(session_id.into(), tx)` unconditionally. If a second `subscribe_session` runs for the same session_id, the second insert silently overwrites the first sender. The first `UnboundedReceiver` (held by the original caller via `UnboundedReceiverStream`) is now disconnected. No error, no log, no panic.
- **Impact:** Reconnect/retry flows cause silent event loss for the older subscription. User sees a frozen conversation pane while the second subscribe call is happily receiving frames.
- **Suggested fix:** Either reject duplicates with a typed error, or merge senders (broadcast), or at minimum log when overwriting. `Err(ClientError::DuplicateSubscription)` is principled.

### crates/forge-tui/src/input.rs:49-52
**TUI Esc on permission modal orphans the prompt**
- **Found by:** bug-hunter (high confidence)
- **Description:** Pressing Esc on the permission modal sets `app.focus = Focus::Conversation` but does NOT clear `app.pending_permission` and does NOT send a deny response. The reverse-RPC remains pending on the daemon side until the 1-hour `HOOK_TIMEOUT_SECS` fires. The orphaned `pending_permission` lingers in App state — modal invisible because focus is Conversation, subsequent Esc no-ops, user has no way to re-open the modal, and the daemon believes the prompt is still being decided.
- **Impact:** Accidental Esc on a permission prompt → user can never answer it from the TUI; the agent stalls until the 1-hour timeout. Looks like a hang.
- **Suggested fix:** On Esc, send a deny response (or call `prompts.respond` with `{"decision": "deny", "reason": "user dismissed"}` for queued prompts) and clear `app.pending_permission`. Mirror `answer_permission` so Esc is symmetric with `d`.

### crates/forged/Cargo.toml:51,56 + crates/forge-tui/Cargo.toml:32,40
**Redundant futures-util in [dependencies] AND [dev-dependencies], not workspace**
- **Found by:** dead-code-analyzer (high confidence)
- **Description:** Both `forged` and `forge-tui` declare `futures-util = "0.3"` in BOTH sections. Cargo treats the dev-dep block as additive only when there's no main-deps entry; with a main entry, the dev line is redundant. Also violates the project's "Workspace-level deps by default" rule.
- **Suggested fix:** Promote to workspace, change crates to `futures-util.workspace = true`, delete the duplicate from `[dev-dependencies]`.

### crates/forged/src/bridged_transport.rs:140-152
**stderr drain in BridgedTransport::spawn collapses EOF and read errors**
- **Found by:** silent-failure-hunter (high confidence)
- **Description:** Detached `tokio::spawn` reading the child's stderr matches `Ok(0) | Err(_)` together — a genuine I/O error is treated identically to clean EOF. Asymmetric with `forge-sdk/src/transport/process.rs::drain_stderr`, which logs the error path explicitly.
- **Impact:** Operators investigating mid-stream session hangs cannot tell whether the CLI exited cleanly or the stderr drainer hit a read error. Stderr is the only diagnostic channel for the subprocess.
- **Suggested fix:** Split the match: `Ok(0) => break` for EOF, `Err(e) => { warn!(error = %e, "claude stderr read failed"); break; }`.

### crates/forged/src/connection.rs:25 + server.rs:94 + registry.rs:114
**Unbounded mpsc channels create OOM cascade (reliability)**
- **Found by:** api-reviewer (high confidence) — re-classified from critical-security to important-reliability under the personal-use threat model.
- **Description:** Per-connection outbound and per-session command channels use `tokio::sync::mpsc::unbounded_channel`. A laptop sleeping mid-session, a frozen terminal, or a network blip blocks the WS receiver indefinitely. With no backpressure, `session.event` notifications, reverse-RPC requests, broadcasts, and dispatch responses queue without limit.
- **Impact:** **Affects single-user.** A sleeping MacBook with an open session can OOM the Mac Studio daemon and kill every other session the user has running. No high-water-mark, no drop-oldest, no slow-subscriber eviction.
- **Suggested fix:** Switch to `mpsc::channel(N)` bounded (e.g. 4096 frames). On send timeout for viewer, evict subscriber and emit `session.role_assigned { reason: "evicted_slow" }`. For primary: drop notifications, deny new prompts, or close the connection. Document the policy in the wire spec.

### crates/forged/src/error.rs:96-102
**Error.data unused — clients regex-parse Display strings**
- **Found by:** api-reviewer (high confidence)
- **Description:** `Error::to_jsonrpc` always sets `data: None`. Three errors carry structured payload that would help clients recover: `ReplayUnavailable { buffer_window_seconds }`, `SessionNotFound(session_id)`, SDK error variants (Process, Connection — exit codes / paths). Today the Display message has the info as plain prose; clients have to regex-parse messages.
- **Suggested fix:** Populate `data` with a JSON object: `{ "buffer_window_seconds": N }` for ReplayUnavailable, `{ "session_id": "..." }` for SessionNotFound, etc. Update wire-spec.

### crates/forged/src/main.rs:73-86
**Multi-bind listener task results un-monitored**
- **Found by:** silent-failure-hunter (medium confidence)
- **Description:** `select_all(handles)` returns the first completing task. The other listeners' tasks are dropped without inspection. If a second bind fails subsequently, the error never surfaces.
- **Impact:** Multi-bind deployments (loopback + WireGuard) lose error visibility on N-1 listeners.
- **Suggested fix:** Iterate `_rest`, abort each, await each handle, log non-cancellation `JoinError` or `Err(_)` via `warn!`.

### crates/forged/src/methods/session.rs (multiple sites)
**Generic InternalError for actor-gone — wrong wire code**
- **Found by:** api-reviewer (high confidence)
- **Description:** When the actor command channel is gone, every handler returns `Error::InternalError("session actor gone")` or `("session actor dropped reply channel")` — sites at session.rs:199-201, 427-429, 956-958, 978-980 (~10+ identical sites). Both surface as -32603 (generic). The defined `Error::SessionNotFound` (-32002) would be the right code.
- **Impact:** Client retry logic cannot decide whether to re-look-up, resubscribe, or treat as a hard failure. Mapping to -32603 leaks actor-pattern detail into the wire shape.
- **Suggested fix:** When actor mpsc returns `SendError` or oneshot returns `RecvError`, unregister the session and return `SessionNotFound`. Optional: add a new `SessionTerminated` variant.

### crates/forged/src/methods/session.rs:262-361 + multi_client.rs:41-160
**Three "subscribe primary takeover" implementations diverge**
- **Found by:** code-simplifier (medium confidence)
- **Description:** `subscribe()` (session.rs:262-361) and `claim_primary()` (multi_client.rs:41-160) carry near-identical primary-takeover sequences (50+ LoC each) with subtly different reason strings. Round 4 fix M3 had to be applied in both places — concrete drift evidence. (Cross-reference: see also the god-file finding below.)
- **Suggested fix:** Extract `set_primary(state, session_id, caller, reason: PrimaryReason)` helper.

### crates/forged/src/methods/session.rs:484-582
**Session actor cancels handle_control mid-write — potential CLI deadlock**
- **Found by:** bug-hunter (medium confidence)
- **Description:** The session actor's `tokio::select! { biased; cmd = commands.recv() => ..., next = client.next_event() => ... }` cancels the `next_event` future when a command arrives. `next_event` internally calls `handle_control(req).await` (forge-sdk/src/client.rs:453) which dispatches MCP/hook/can_use_tool requests and writes a `control_response` via `self.sub.write_line(&line).await`. If `select!` cancels `next_event` while `handle_control` is mid-write_line (waiting for BridgedTransport writer's oneshot ack), the response is never sent. CLI then waits indefinitely for a reply — session deadlock. This contradicts the actor pattern's documented rationale (`project_forged_actor_pattern.md`) which was supposed to prevent exactly this.
- **Impact:** When the daemon dispatches a control_request from CLI (hook callback, MCP bootstrap, can_use_tool) AND a wire command arrives simultaneously, control_response may be lost and the CLI session hangs forever.
- **Suggested fix:** Wrap `client.next_event()` in a poll-once helper that runs to completion once started (e.g. hold a shared `next_event` future across loop iterations using `Pin<Box<...>>` with `&mut Future`). Alternative: run `handle_control` on a separate task that owns its own write path. Or make `select!` non-biased + interlock deferring command processing while `handle_control` is in flight.

### crates/forged/src/methods/session.rs (god file)
**1053 LoC mixing four responsibilities**
- **Found by:** architecture-reviewer (high confidence). Cross-references: code-simplifier (subscribe vs claim_primary takeover dup, session.rs:262-361) and code-simplifier (12 actor-call template sites across session.rs/mcp.rs/context.rs).
- **Description:** Single file holds four distinct responsibilities: (a) wire-RPC handler thin proxies (lines 56–458, 949–1053); (b) the session actor body `spawn_session_actor` at lines 460–606 (146-line `tokio::select!` loop); (c) complete mirror of `forge_sdk::Options` as wire structs `WireOptions`, `WireSystemPrompt`, `WireTools`, `WireThinking`, `WireEffort`, `WirePlugin` at 612–742; (d) a 170-line `parse_spawn_params` deserializer at 763–937. CLAUDE.md style note says extract sub-modules when a file crosses ~500 LoC OR has distinct responsibilities. Two `=============` section dividers + two `#[allow(clippy::too_many_lines)]` are the smell signal.
- **Impact:** Shotgun surgery (Options field changes touch WireOptions + parse_spawn_params + SDK Options). Cognitive load. Test isolation harder.
- **Suggested fix:** Split into `methods/session/` directory: `methods/session.rs` (handler proxies, ~300 LoC), `methods/session/actor.rs` (`spawn_session_actor` + helpers), `methods/session/wire_options.rs` (`WireOptions` + `parse_spawn_params`; reusable — forged-conformance already cross-imports it).

### crates/forged/src/methods/session.rs:786-798 + server.rs:527-539
**Duplicated parse_permission_mode in two places**
- **Found by:** code-simplifier (high confidence)
- **Description:** Two functions independently translate the same six wire-strings → `PermissionMode`. Drift hazard: a new mode means updating both.
- **Suggested fix:** Move to one place (`session_state.rs` or `wire_enums.rs`). Better: implement as `serde::Deserialize` on a wire-shape enum used by both `WireOptions` and `SetPermissionModeParams`.

### crates/forged/src/registry.rs:117,124,138,200
**usize counter arithmetic in registry — fragile pattern**
- **Found by:** bug-hunter (medium confidence)
- **Description:** `*self.active_sessions.lock() += 1 / -= 1` and `*self.connected_clients.lock() += 1 / -= 1` use direct usize arithmetic. Decrements are guarded by `if .remove().is_some()`. The pattern is fragile; the counters are only used for `daemon.status`. If any future code path adds a divergent register without bump, the counter underflows (panic in debug, wrap to `usize::MAX` in release).
- **Impact:** Status reporting could show wildly incorrect session counts; in debug, an overflow could crash the daemon.
- **Suggested fix:** Use the map's `len()` directly when reporting status. Or `usize::saturating_sub(1)` and `AtomicUsize`. Removing the counter is cleanest — `HashMap::len()` is O(1) and always correct.

### crates/forged/src/reverse_rpc.rs:42-95
**reverse_rpc insert-before-send race**
- **Found by:** bug-hunter (medium confidence)
- **Description:** In `try_send_to_primary`, the responder is inserted into `outstanding_reverse` BEFORE the request is sent to the primary's outbound channel. If the channel send fails (line 79-81), the function tries to peel back via `state.outstanding_reverse.lock().remove(rev_id)` (line 85). Between insert (line 61) and remove (line 85), if a replayed inbound `rev_<uuid>` response arrives at the read_loop, it could pull the entry out and resolve a responder for a request that was never actually sent.
- **Impact:** Even in a single-user setting, an accidentally-replayed response (e.g. retry on a flaky link, daemon restart racing client retransmit) can resolve a reverse-RPC the primary never received, leading to permission decisions or hook outcomes derived from stale input.
- **Suggested fix:** Reorder so `send` happens FIRST. Only insert into `outstanding_reverse` after `out.send(...)` succeeds.

### crates/forged/src/reverse_rpc.rs:251-320
**outstanding_reverse unbounded (reliability)**
- **Found by:** api-reviewer (high confidence)
- **Description:** `issue_to_primary` has a global `outstanding_reverse` HashMap with no upper bound. For every spawned session without a primary, every `permission.request` and `hook.<kind>` callback parks for up to `HOOK_TIMEOUT_SECS = 3600`. Each entry pins a `oneshot::Sender` + the original params blob.
- **Impact:** **Affects single-user.** If the user's primary client (laptop) is asleep and hook-heavy traffic continues on a spawned session, memory grows for an hour before the timeouts cull. Same backpressure shape as the unbounded mpsc finding above.
- **Suggested fix:** Cap `outstanding_reverse` at e.g. 1024 daemon-wide (or per-session 16). When at cap, reject with security-critical fail-closed semantics. Surface as `Error::Overloaded`.

### crates/forged/src/server.rs:225-231
**Non-string id silent drop**
- **Found by:** api-reviewer (medium confidence)
- **Description:** When the daemon receives a JSON-RPC response frame with a non-string id, it logs `warn` and drops it. JSON-RPC 2.0 §4.2 explicitly allows string OR number. Today the reverse-RPC issuer always uses `rev_<uuid>` (string), but warn-and-drop is fragile for protocol extension.
- **Suggested fix:** Either accept numeric ids by stringifying for lookup, OR document loud-and-clear in the wire spec + surface a JSON-RPC error response (-32600) instead of pure log.

### crates/forged/src/server.rs:300-513
**Dispatch table boilerplate (200 lines, 25 arms with same shape)**
- **Found by:** code-simplifier (high confidence)
- **Description:** 200-line dispatch repeats `match parse_params...` with `Err(e) => Err(e)` arm as pure noise. ~40% plumbing.
- **Suggested fix:** Extract a `typed_call`/`typed_call_unit` helper pair. Each arm shrinks from 4 lines to 1. Saves ~120 LOC.

## Minor Findings

### crates/forge-conformance/src/session_redact.rs:111,142
**RedactState and transform_persistence_line documented-but-internal**
- **Found by:** dead-code-analyzer (medium confidence). `pub struct RedactState` and `pub fn transform_persistence_line` have only intra-file callers. README mentions them but no test or external crate calls them. Either add a test exercising them, or demote to `pub(crate)`.

### crates/forge-sdk/Cargo.toml:35
**forge-sdk dev-dep tracing-subscriber pinned independently**
- **Found by:** dead-code-analyzer (high confidence). `tracing-subscriber = { version = "0.3", features = ["env-filter"] }` — version pinned manually; workspace declares same version with features `["env-filter", "fmt"]`. Drift risk + subtle feature flag mismatch. Replace with `tracing-subscriber = { workspace = true }`.

### crates/forge-sdk/src/messages.rs (1681 LoC)
**Largest file in workspace — watch item, do not split preemptively**
- **Found by:** architecture-reviewer (medium confidence). Cohesive (stream-json wire contract). Splitting risks scattering wire-dispatch logic. User-facing types (lines 22–488) and wire-shim layer (~490–760) are cleanly separable. If a future parity round adds another large variant, promote to `messages/` directory.

### crates/forge-sdk/src/session/scan.rs (1049 LoC)
**Large but on right side of SRP — promote when SessionStore variants land**
- **Found by:** architecture-reviewer (medium confidence). Cohesion is high, currently at the SRP boundary. The TODO at line 21 says SessionStore-backed variants will land — those will push past 1200+ LoC. When `*_from_store` variants land, promote to `session/scan/` directory.

### crates/forge-tui/Cargo.toml:36
**forge-tui dev-dep on forge-sdk is unused**
- **Found by:** dead-code-analyzer (high confidence). `forge-sdk = { path = "../forge-sdk" }` in `[dev-dependencies]`. Zero direct references in forge-tui's `src/` or `tests/`. Remove the line.

### crates/forge-tui/src/client/connection.rs:511-522
**Single-impl OptionExt trait**
- **Found by:** code-simplifier (high confidence). Trait `OptionExt::cloned_kind()` impl'd exactly once for `Option<&ReverseHandlerKind>`, used at exactly one call site (line 393). Replace with an inherent method on `ReverseHandlerKind` plus `.map`. Or simply `#[derive(Clone)]` since the inner `Arc`s are already cheap.

### crates/forge-tui/src/main.rs:174,178
**TUI sessions.list send drops are silent**
- **Found by:** silent-failure-hunter (medium confidence). Both branches use `let _ = tx.send(...)`. Failure branch hides original `e` AND the send failure. No symmetry-trace following the round-5 convention. Add `if tx.send(...).is_err() { tracing::trace!("sessions.list result dropped — receiver gone"); }` per server.rs:204-216 convention.

### crates/forge-tui/src/ui.rs (4 files, 192 LOC)
**ui/ split borderline — leave alone**
- **Found by:** code-simplifier (medium confidence). 192 LOC across 4 files. `conversation::render_message` (40 LoC) called from one site. `permission_modal::render` (30 LoC) from one site. Inlining into one ui.rs would shrink it, but borderline — leave alone if M0 wants growth room.

### crates/forged-conformance/Cargo.toml:14
**Unused async-trait dev-dependency**
- **Found by:** dead-code-analyzer (high confidence). `async-trait.workspace = true` declared but zero matches for `async_trait` in `src/` or `tests/`. Remove the line.

### crates/forged-conformance ↔ forged
**forged-conformance reaches into forged internal modules**
- **Found by:** architecture-reviewer (medium confidence). Test crate imports private-shaped types from forged's internal modules: `registry::DaemonState`, `session_state::SessionId`, `reverse_rpc::issue_to_primary`, `prompt_queue::PromptKind`, `methods::session::SpawnResult/SubscribeResult/parse_spawn_params`, `methods::multi_client::PeersResult`. Every `forged::*` is `pub mod` purely so the conformance crate can poke at it. Refactoring inside forged silently breaks conformance; the internal/public split is erased. Either (a) document forged is a binary with internal modules exposed for the harness, OR (b) add `forged::test_api` re-export module gated `cfg(any(test, feature = "test-api"))`. Recommend (a).

### crates/forged/src/bind_check.rs:24
**bind_check starts_with too lenient**
- **Found by:** bug-hunter (high confidence). `bind.starts_with("localhost")` matches `localhost.evil.com:7373`, `localhostfoo:7373`. Similarly `127.` matches `127.0.0.1.evil.com`. Check is advisory (drives WARN log on non-loopback) — false positive misleads diagnostic logs. Use `bind == "localhost" || bind.starts_with("localhost:")`. Same for `127.` → ensure octet boundary.

### crates/forged/src/bridged_transport.rs:204
**BridgedTransport::end_input masks ack-channel drops**
- **Found by:** silent-failure-hunter (medium confidence). `ack_rx.await.unwrap_or(Ok(()))` collapses successful ack and dropped-channel cases. Writer panic during EndInput reports success; the next `write_line` fails confusingly. Use `ack_rx.await.unwrap_or_else(|_| { tracing::warn!("..."); Ok(()) })`.

### crates/forged/src/connection.rs:32-35
**Redundant Connection::new vs with_metadata**
- **Found by:** code-simplifier (medium confidence). `Connection::new` exists only to forward to `with_metadata(id, None, SystemTime::now(), outbound)`. Two ctors for the same type, one a thin shim. Drop one or merge.

### crates/forged/src/iso8601.rs:38, logging.rs:89, registry.rs:211, reverse_rpc.rs:169, sdk_callbacks.rs:239,247
**forged crate pub fns with no external consumers**
- **Found by:** dead-code-analyzer (high confidence). `pub fn secs_to_ymdhms`, `pub fn expand_home`, `pub fn purge_connection_from_sessions`, `pub fn notify_disconnect_expired`, `pub fn is_security_critical`, `pub fn fail_closed_decision` — all `pub fn` with zero callers outside the defining file. Reduce to `pub(crate)` or drop `pub`.

### crates/forged/src/jsonrpc.rs:111-132
**JsonRpcVersion manual Serialize/Deserialize for one literal**
- **Found by:** code-simplifier (medium confidence). 15 LOC of hand-rolled serde for what's a single-variant enum with `#[serde(rename = "2.0")]`. Replace with derived single-variant enum. ~5 LOC.

### crates/forged/src/logging.rs:77 (also line 49)
**forged logging init drops failures**
- **Found by:** silent-failure-hunter (medium confidence). `let _ = Registry::default()...try_init()`. If a subscriber is already initialised, the failure is swallowed. Future emissions go to a stale subscriber. Match on the result; warn-via-eprintln on Err (only escape since tracing failed).

### crates/forged/src/methods/session.rs:56-91
**No cap or rate limit on session.spawn (self-foot-gun)**
- **Found by:** api-reviewer (high confidence) — re-classified from critical to minor under the personal-use threat model. The user can `pkill claude` if they accidentally loop a spawn script. Worth noting because `Error::RateLimited` and `Error::Overloaded` exist in the error type but are never returned — if/when the trust model expands, this is the first thing to wire up.

### crates/forged/src/methods/session.rs:107-133
**SpawnParams has From<Options>, from_options, AND Deref<Target=Options>**
- **Found by:** code-simplifier (medium confidence). Three accessors for what's effectively `Options` + an extra `hooks` field. `Deref` is documented as "so existing tests can read fields directly" — test-driven Deref is a code smell. Drop `Deref` and `from_options`. Update tests to read `params.options.binary`. Keep `From<Options>`.

### crates/forged/src/methods/session.rs:171-202
**send_user_message accepts unbounded prompt (self-foot-gun)**
- **Found by:** api-reviewer (high confidence) — re-classified from important to minor under the personal-use threat model. The user is unlikely to type-paste a 1 GB prompt at themselves. Holding three copies of any large prompt (incoming WS buffer, Request::params, Command enum) is still wasteful; a 1 MiB cap costs nothing. Useful as defensive hygiene, not security.

### crates/forged/src/methods/session.rs:219-225
**SubscribeParams::since silently ignored**
- **Found by:** api-reviewer (medium confidence). `since` field documented but the implementation accepts any string and discards it (`_since: Option<String>`). No validation, no rejection, no surface of "replay unavailable." `Error::ReplayUnavailable (-32007)` exists precisely for this. Either surface `Error::ReplayUnavailable { buffer_window_seconds: 0 }` whenever `since.is_some()` and there's no replay buffer, or accept and validate cursor format and document M5 stub status.

### crates/forged/src/methods/session.rs:543-648
**Per-method dispatch param structs duplicate forge-sdk shapes**
- **Found by:** code-simplifier (medium confidence). 12 single-use `*Params` structs for dispatch holding 1-4 fields, several textually identical. Method modules also define their own `SubscribeParams`/`SendUserMessageParams`/etc. — no consistent rule on where param structs live. Pick one location per method (the methods module is more natural — co-located with handler). Saves ~80 LOC.

### crates/forged/src/methods/session.rs:625-671
**WireOptions deny_unknown_fields confusing for can_use_tool**
- **Found by:** api-reviewer (medium confidence). `WireOptions` uses `deny_unknown_fields`. The spawn handler silently ignores wire-irrelevant fields (can_use_tool, hooks_callback, in-process MCP) by not declaring them. A client typing those gets a confusing "unknown field 'can_use_tool'" rather than "this field has no wire representation." Declare a sentinel set of intentionally-unsupported names and reject with explicit `InvalidParams`. Or document the wire-supported subset.

### crates/forged/src/methods/sessions.rs:37-43
**sessions.list unbounded result set (self-foot-gun)**
- **Found by:** api-reviewer (high confidence) — re-classified from important to minor under the personal-use threat model. With thousands of historical transcripts the result set could be large, but the only consumer is the user's TUI which can ask for what it needs. A default `limit=200` is still good ergonomics.

### crates/forged/src/methods/{session.rs,mcp.rs,context.rs}
**12 actor-call boilerplate functions match same 8-line template**
- **Found by:** code-simplifier (medium confidence). Cross-reference: god-file finding for session.rs above. Every actor-routed handler follows the same shape: get session, oneshot, send command, await reply. 12 sites with the same 8-line template; only Command variant ctor and return type vary. Generic `dispatch_command<R>(state, session_id, build: impl FnOnce(...))` helper. Saves ~50 LOC.

### crates/forged/src/sdk_callbacks.rs:238-253
**Two security-critical-list policy functions when one suffices**
- **Found by:** code-simplifier (medium confidence). `is_security_critical(kind)` exported but only used internally by `fail_closed_decision`. Bool predicate exists only to factor a match out of dispatch. Inline or make private. `pub` is YAGNI.

### crates/forged/src/server.rs:543-648
**session_id type inconsistency**
- **Found by:** api-reviewer (high confidence). Most newer methods deserialize `session_id` as `crate::session_state::SessionId` (typed). The `sessions.*` family uses bare `String`. Same JSON shape but consumer-side deserialization, error messages, and future stricter validation diverge. Use `SessionId` in all param types.

### Result-shape inconsistency across methods (multiple files)
- **Found by:** api-reviewer (high confidence). `session.spawn` returns `{ session_id }` directly; `sessions.list` wraps `{ sessions: [...] }`; `sessions.list_subagents` wraps `{ subagent_ids: [...] }`; `sessions.subagent_messages` wraps `{ messages: [...] }`; `sessions.messages` returns typed `MessagesResult`; `sessions.project_key` wraps `{ project_key: "..." }`; `sessions.info` returns bare `Option<SDKSessionInfo>` (null when missing); `sessions.fork` returns `{ session_id }`; `session.peers` wraps `{ peers: [...] }`. Pick one convention — typed result struct with named fields for every method.

## Deferred follow-ups (applied in second pass)

These findings are real but were not addressed in the 2026-04-26
cleanup branch — either the fix is design-heavy enough to warrant a
focused effort, or the savings don't justify the churn. Tracked here
so a future cleanup can pick them up.

- **`methods/session.rs` god file split** (architecture-reviewer,
  important) — file dropped from 1053 → ~930 LoC after the
  `dispatch_command` and `become_primary` refactors removed the
  inline boilerplate. Still on the wrong side of the 500-LoC
  guideline. Split into `methods/session/{actor,wire_options}.rs`
  is mechanical but multi-file; deferred.
- **Bounded mpsc channels with slow-subscriber eviction**
  (api-reviewer, important reliability) — the `outstanding_reverse`
  cap landed (1024 entries, returns Overloaded). The per-connection
  outbound and per-session command channels are still
  `mpsc::unbounded_channel` though — bounding them needs a coherent
  eviction policy (drop-oldest? evict the slow subscriber? close the
  connection?) plus role-aware behaviour for the primary vs viewer
  case. Deferred until the policy is designed.
- **Dispatch table `typed_call` helper** (code-simplifier,
  important) — the `match parse_params { Ok(p) => ..., Err(e) =>
  Err(e) }` shape across ~25 arms in `server.rs::dispatch` is
  verbose but readable; the typed_call helper would change every
  arm and save ~25 LoC of "Err(e) => Err(e)" lines. The savings
  don't clearly justify the churn relative to other items in this
  audit. Deferred.

## Removed under personal-use threat model

These findings from the original specialist reports were dropped because their severity required an adversarial assumption that does not hold for forge:

- **Cross-client authorization gap** (`methods/session.rs:184-202` and many sites) — every session-mutating method ignores caller identity. This is the **intended UX** for multi-machine session takeover; not a finding.
- **Path traversal via `directory` parameter** (`sessions.rs:50-114`) — caller-supplied directory not canonicalized vs `projects_dir`. There is no malicious caller; the user won't path-traverse against themselves.
- **WS upgrade/recv error info leak** (`server.rs:88,179`) — tungstenite errors include local file paths in the message. Operator and client are the same person.
- **claim_primary takeover-DoS** (`multi_client.rs:41-159`) — any subscriber can take primary with no consent. **Takeover IS the feature** in personal-use multi-machine workflow. Cool-down windows would actively get in the way.

If the trust model expands (collaborator access, public deployment, multi-user) these findings should be re-introduced as critical. Re-run the audit with the updated `project_trust_model.md` context.
