# Peer MCP - cross-agent coordination

Every spawned `claude` child gets an in-process MCP server (`mcp__forge__*`) exposing four tools - `peers__whoami`, `peers__list_agents`, `peers__tell_agent`, `peers__ask_agent`. The forge.toml project name is the agent identity (one session per project). When the LLM in project **A** calls `ask_agent`, forge wraps the prompt in a bracket-prefixed envelope (e.g. `[Question id=q-XXXXXXXX from agent 'A' (org 'Personal') - reply with tell_agent in_reply_to=q-XXXXXXXX]`) and dispatches it as a synthetic user turn to project **B**. B's reply lands as another wrapped envelope on A's chat. The chat renderer pattern-matches the wrappers at render time and shows a styled **peer block** instead of the raw bracket prose.

Tool calls for `mcp__forge__peers__*` are auto-approved by `forge-sdk::control_dispatch` - the LLM never sees a permission prompt for peer coordination. The default tool-use card is suppressed via `hidden: true` on `ToolCallInfo` so the chat shows the styled **outbound peer block** in its place. See the [Projects pane](./projects-pane.md) for the per-row in-flight badge cluster.

## Peer / worker chat blocks - one shape for every inbound + outbound envelope

*visible: every `mcp__forge__peers__*` / `mcp__forge__workers__ask|tell` tool call the LLM emits, and every inbound wrapped envelope (`[Question ...]`, `[Message ...]`, `[Reply ...]`, `[Late reply ...]`, `[Ask ... timed out ...]`, `[Ask ... has expired ...]`, `[Ask ... failed to deliver: ...]`, `[Worker ... spawn failed ...]`)*

Five TitleCase verbs cover every variant; the verb names the kind, a leading directional kind-icon names the direction. Outbound (`Tell`, `Ask`) carries `⤴` (U+2934); inbound (`Message`, `Question`, `Reply`) carries `⤵` (U+2935). Each row reads `▶ ⤴ Verb name` (outbound) or `▶ ⤵ Verb name` (inbound) with the body indented under the standard tool-card tree connectors (`│  ` for continuation, `└─ ` for the last line). No source label sits above them: the row names its own kind and peer, so a label restating it is redundant weight.

| Verb | Direction | Wire shape |
|---|---|---|
| `Tell` | outbound unsolicited | `workers__tell` / `peers__tell_agent` |
| `Ask` | outbound question | `workers__ask` / `peers__ask_agent` |
| `Message` | inbound unsolicited | `[Message id=... from agent ...]` |
| `Question` | inbound question | `[Question id=... from agent ...]` |
| `Reply` | inbound response | `[Reply id=... from agent ...]` · `[Late reply ...]` carries a `⚠ late` modifier |

<div class="term">

  <pre class="indent">
     <span class="accent bold">▶</span> <span class="dim bold">⤴</span> <span class="bold">Tell planner</span>
     <span class="dim">└─ Re-stating tight: #178 go option A, #179+#180 brainstorm, #182 opt-in.</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤴</span> <span class="bold">Ask planner</span>
     <span class="dim">└─ Is the seam plan ready for the workspace.rs split?</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Message planner</span>
     <span class="dim">└─ Plan for #185 sent to implementer with background-priority note (queue...</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Message implementer</span>
     <span class="dim">│  PR #187 open (draft, closes #184). Worker-resume tag-path bug fix;</span>
     <span class="dim">│  reviewer pinged. Heads-up: #187's diff shows -105 lines as artifact...</span>
     <span class="dim">└─ Queued: #185 after these two land.</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Reply planner</span>
     <span class="dim">└─ Yes - seam plan attached, six carve-outs roughly 300 LOC each. Sign-of...</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Question data-modules</span>
     <span class="dim">└─ Should we proceed with the runner upgrade now or wait for CI to drain?</span></pre>

</div>

Notices stay single-line with a `⚠` modifier inline.

<div class="term">

  <pre class="indent">
     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Ask planner</span> <span class="dim">-</span> <span class="warning">⚠ timed out</span>
     <span class="dim">└─ was: "Is the seam plan ready for the workspace.rs split?"</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Tell planner</span> <span class="dim">-</span> <span class="warning">⚠ undeliverable</span>
     <span class="dim">└─ reason: target sleeping</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Reply planner</span> <span class="dim">-</span> <span class="warning">⚠ late</span>
     <span class="dim">└─ Sorry for the delay - the original ask had already expired.</span>

     <span class="accent bold">▶</span> <span class="dim bold">⤵</span> <span class="bold">Question forge</span> <span class="dim">-</span> <span class="warning">⚠ expired</span>
     <span class="dim">└─ your reply will be tagged late.</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/peer_block.rs::detect_inbound` + `detect_outbound` + `render_block` (one renderer takes a TitleCase verb + name + optional modifier + body) · invoked from `ui::message::append_assistant_tool_block` + `append_user_blocks`
- **collapse / expand** - body ellipsed to one line by default (collapsed shape: `└─ <first 60 chars>...`); click the row to expand the full body inline. Same affordance pattern the existing peer block uses for collapsed-by-default
- **same-worker streak** - three consecutive envelopes from the same worker (per `chat::group_envelope_streak`) stack body lines under one header - no repeated `▶ Message <same-name>` rows. Same-project envelope streaks (different workers in the same project) still get one header per worker
- **variants** - 5 main verbs (`Tell` · `Ask` · `Message` · `Question` · `Reply`) + 4 notice modifiers (`⚠ timed out` · `⚠ undeliverable` · `⚠ late` · `⚠ expired`) + 2 directional kind-icons (`⤴` outbound · `⤵` inbound) in the slot between the row glyph and the verb. Outbound covers `render_outbound` (Ask / Tell); inbound covers every `render_inbound` arm including the timeout / delivery-failure notices whose original asks were ours but whose notice envelopes arrived inbound. `[Worker ... spawn failed ...]` stays as a one-line system notice with no kind-icon (workspace-generated lifecycle event, not a peer comm)
- **dropped from prior shape** - status glyph (`✓` / `⚠` / `✗`), kind icons (`←` / `→` / `↩`), correlation id chrome (`· q-...`), `(org)` meta on same-org rows, the `(worker in ...)` identity suffix. `workers__spawn` / `workers__list` revert to standard tool_call rendering (they're worker lifecycle, not peer comm)
- **parser shape** - pure prefix string-match + manual field extraction (`take_until`, `rest_after_id`, `id_before`, `extract_from_agent_after`). No regex. Malformed envelopes fall through to the default user-message rendering rather than erroring
- **suppression** - `crates/forge-tui/src/ui/tool_call.rs` sets `ToolCallInfo.hidden = true` for `mcp__forge__peers__*` and `mcp__forge__workers__ask` / `workers__tell`; `workers__spawn` / `workers__list` render as standard tool cards
- **arrival order + running state** - an inbound peer / worker turn appends at the **tail** in arrival order - never repositioned above the in-flight assistant turn that holds the outbound send. Delivery then mirrors `input_submit::dispatch_prompt` exactly: strip any stranded empty placeholder (so rapid back-to-back delivery - a Gotify flood - never leaves a blank bubble between turns), open a fresh empty assistant placeholder at the tail, and reparent the active-turn pointer onto it (`App::push_active_turn_assistant_placeholder`) so the thinking spinner pins to the bottom above the input - not on a stale earlier assistant near the top - then flip the session to a running state (chat spinner + [Projects pane](./projects-pane.md) spin) so it reads as active rather than idle-then-burst. All live in `sdk_message::push_peer_envelope_user_turn_if_present` (gated on `!replay_in_progress` so a resumed history doesn't open a live turn or stick the spinner)

## Gotify notification chat block - inbound external notifications

*visible: every matched Gotify notification delivered into a subscribed session (project lead or a team worker), echoed as an inbound external-notification block at the tail, ahead of the response it triggers*

A matched notification is delivered as a user turn (every notification is its own turn) and echoed into the chat so the user sees what arrived. It reads unambiguously as an external notification, not agent traffic: a Gotify source label where peer / worker traffic carries none, the ◈ gotify glyph (◈, not ▶), and an `app 'X' - priority N` header. The body carries the notification title then message under the standard tree connectors (`│  ` continuation, `└─` last).

<div class="term">

  <pre class="indent">
   <span class="gotify bold">Gotify</span>

     <span class="gotify bold">&#x25C8;</span> <span class="bold">app 'Backups'</span> <span class="dim">- priority</span> <span class="dim">3</span>
     <span class="dim">&#x2502;&nbsp;&nbsp;Nightly backup complete</span>
     <span class="dim">&#x2514;&#x2500; All volumes backed up (2.4 TB in 41m)</span>

     <span class="gotify bold">&#x25C8;</span> <span class="bold">app 'Security-Watchdog'</span> <span class="dim">- priority</span> <span class="warning">8</span>
     <span class="dim">&#x2502;&nbsp;&nbsp;Failed login attempt</span>
     <span class="dim">&#x2514;&#x2500; 3 failed SSH logins from 192.0.2.5 in 60s</span></pre>

</div>

The priority number renders in warning at or above 5, otherwise dim - a quick severity cue.

- **code** - `crates/forge-tui/src/ui/peer_block.rs::detect_inbound` parses the `[Gotify - app '...', priority N]` prefix into `PeerInboundKind::Gotify`; `render_inbound` routes it to `render_gotify_notification`. The Gotify source label comes from `role_label_line` reading the cached `ChatMessage.is_gotify_envelope` flag (stamped at push time, mirroring the peer-envelope flag)
- **data source** - `SessionUpdate::GotifyNotificationAppended { session_id, notification }` emitted by `spawn::push_gotify_notification_into_chat` at each delivery site (running lead / running team worker) and on the Connected drain for a spawned-to-deliver notification. The reducer forges a synthetic user turn from `GotifyNotification::to_prose()` - the same prose the session's LLM receives via `Command::Prompt`, so the existing `detect_inbound` matcher recognises it
- **collapse / expand** - body ellipsed to one line by default (`└─ <first 60 chars>...`); click the row to expand the full body inline - same affordance as the peer block
- **distinct from peer** - a Gotify notification is an external event, not agent traffic: it never merges into a peer messaging group (`PeerInboundKind::peer_sender_identity` returns `None` for it), keeps its own Gotify source label, and carries the ◈ gotify glyph in place of ▶
- **arrival order + running state** - appends at the tail in arrival order, opens a fresh assistant placeholder at the tail so the thinking spinner pins to the bottom above the input, and flips the session to a running state on delivery - the same `push_peer_envelope_user_turn_if_present` path the **peer block** uses

## Cron chat block - fired scheduled prompts

*visible: every durable cron that fires into its owner (the project lead or a worker, woken if asleep), echoed as a cron block at the tail, ahead of the response it triggers*

A due cron is delivered as a user turn (the session's LLM receives the raw prompt) and echoed into the chat so the user sees what fired instead of the agent bursting into a response from nowhere. It reads as an internal scheduled event, not typed input: a Cron source label where peer / worker traffic carries none, the ◴ cron glyph (◴, the same glyph the [SCHEDULES](./inspector.md) section uses, not ▶), and the fired prompt under the standard tree connectors (`│  ` continuation, `└─` last). An overdue fire (forge or the owner was down through the scheduled minute) prefixes the prompt with a plain `[missed cron]` marker so a catch-up reads apart from an on-time fire.

<div class="term">

  <pre class="indent">
   <span class="accent bold">Cron</span>

     <span class="accent bold">&#x25F4;</span> <span class="bold">Cron</span>
     <span class="dim">&#x2514;&#x2500; run the morning market summary</span>

     <span class="accent bold">&#x25F4;</span> <span class="bold">Cron</span>
     <span class="dim">&#x2502;&nbsp;&nbsp;post the EOD report to #trading</span>
     <span class="dim">&#x2514;&#x2500; include the day's realized PnL</span></pre>

</div>

The `[Cron]` wrapper is display-only: it exists solely to drive the visible block and inherit the delivered-turn spinner. The subprocess receives the raw prompt via `Command::Prompt`, so the bracket never reaches the LLM.

- **code** - `crates/forge-tui/src/ui/peer_block.rs::detect_inbound` parses the `[Cron]` prefix into `PeerInboundKind::Cron`; `render_inbound` routes it to `render_cron_prompt` (the ◴ glyph in `RUST_ORANGE`). The Cron source label comes from `role_label_line` reading the cached `ChatMessage.is_cron_envelope` flag (stamped at push time, mirroring the peer / gotify envelope flags)
- **data source** - `SessionUpdate::CronPromptAppended { session_id, text }` emitted by `spawn::push_cron_prompt_into_chat` at the running-lead delivery site and on the Connected drain for a cron fired into an asleep project. The reducer forges a synthetic user turn wrapping the fired prompt in the display-only `[Cron]` prefix, so the existing `detect_inbound` matcher recognises it
- **collapse / expand** - body ellipsed to one line by default (`└─ <first 60 chars>...`); click the row to expand the full prompt inline - same affordance as the peer / gotify blocks
- **distinct from peer** - a fired cron is a scheduled internal event, not agent traffic: it never merges into a peer messaging group (`PeerInboundKind::peer_sender_identity` returns `None` for it), keeps its own Cron source label, and carries the ◴ cron glyph in place of ▶
- **arrival order + running state** - appends at the tail in arrival order, opens a fresh assistant placeholder at the tail so the thinking spinner pins to the bottom above the input, and flips the session to a running state on delivery - the same `push_peer_envelope_user_turn_if_present` path the **peer** / **gotify** blocks use

## Sidebar peer-activity badges (Projects pane)

*visible: per-row on every live (active) project row AND on every worker row in the [Projects pane](./projects-pane.md) when any of the four counters is non-zero*

Badge cluster between the row name and the close ` x ` button. Each badge is `·N` + glyph; counts of 0 are omitted entirely (the goal is "noise only when there's activity"). Failure badges (`⌛`, `✕`) disappear 60 s after the counter last incremented so a one-time spawn hiccup doesn't paint the sidebar red forever. Workers carry their own per-session badges - a forge-asks-worker bumps the worker's `incoming`, a worker-asks-sibling bumps the worker's `outgoing`, so each row's counter reflects that row's own pending asks.

<div class="term">

  <pre class="indent">
 <span class="dim">└─ </span><span class="accent">⠋</span> <span class="accent bold">forge</span> <span class="dim">·2↑·1↓</span>             <span class="user-band"> x </span>
 <span class="dim">   │  ├─ </span><span class="dim">⠋</span> <span class="dim">probe-a</span>  <span class="dim">·1↑</span>             <span class="user-band"> x </span>
 <span class="dim">   │  └─ </span><span class="dim">⠋</span> <span class="dim">reviewer</span>                      <span class="user-band"> x </span>
 <span class="dim">└─ </span><span class="accent">⠋</span> <span class="accent bold">gateway-backend</span> <span class="dim">·1↓</span><span class="warning">·1⌛</span> <span class="user-band"> x </span>
 <span class="dim">└─ </span><span class="accent">⠋</span> <span class="accent bold">gateway-liq-bot</span> <span class="error">·1✕</span>     <span class="user-band"> x </span></pre>

</div>

| Badge | Source field | Color | Meaning |
|---|---|---|---|
| `·N↑` | `PeerInflightStats.outgoing` | DIM | Asks this session sent that are still awaiting reply |
| `·N↓` | `PeerInflightStats.incoming` | DIM | Asks this session received that are still awaiting our reply |
| `·N⌛` | `PeerInflightStats.timed_out` | STATUS_WARNING | Asks this session sent that timed out (30-min default). Fades after 60 s. |
| `·N✕` | `PeerInflightStats.delivery_failed` | STATUS_ERROR | Asks this session sent that failed to deliver (spawn / channel / connection error). Fades after 60 s. |

- **code** - `crates/forge-tui/src/ui/projects_pane.rs::peer_badge_spans` · called from `append_org_project_row` (lead rows) AND `append_worker_tree_children` (worker rows) · reads `UiSession.peer_badges` + `peer_badges_last_failure_at` keyed by each row's own `session_key`
- **wire** - `SessionUpdate::PeerInflightStatsChanged { key, stats }` from `forge-workspace` · reducer arm in `crates/forge-tui/src/app/events/client.rs` stamps both fields atomically
- **fade window** - `PEER_FAILURE_FADE = Duration::from_secs(60)` · failure badges (`·N⌛` / `·N✕`) hide once `now - peer_badges_last_failure_at >= 60 s`
- **idle rows** - no badge column. Sleeping projects have no `UiSession` bucket to read peer state from; the full name column gets the budget instead

## Tools exposed by the `mcp__forge__` server

*scope: every spawned `claude` child. Auto-approved (no permission prompt)*

| Tool | Inputs | Semantics |
|---|---|---|
| `peers__whoami` | - | Returns the caller's own `PeerStatus` (project name, org, cwd, current model, in-flight outgoing/incoming counts). |
| `peers__list_agents` | - | Returns every project listed in `forge.toml` with its current liveness (running / sleeping / failed), current model, and in-flight counters. The caller's own row is included so the LLM can reason about its own identity in the list. |
| `peers__tell_agent` | `target: string` · `message: string` · `in_reply_to: Option<string>` | Fire-and-forget message. Returns immediately with the synthesised `correlation_id` (`t-XXXXXXXX`). When `in_reply_to` is set, the workspace looks up the matching ask: a reply to a still-open ask renders as `Reply` and closes the ask; a reply whose target doesn't match (LLM hallucinated the wrong target) or whose id doesn't exist (stale / already answered) demotes to `Message` with a warn log and a `note` in the tool result so the replier can retry with the right id. |
| `peers__ask_agent` | `target: string` · `prompt: string` | Returns immediately with a `correlation_id` (`q-XXXXXXXX`) and the ask transitions to in-flight. The reply lands as a synthetic user turn on the caller via the `[Reply ...]` envelope. The ask stays in-flight until a reply lands or the target is lost. `UnknownTarget` returns `is_error: true` synchronously; async failures (target spawn failed / channel closed / target connection died) fire `SessionUpdate::PeerAskFailed` + a wrapped `[Ask ... failed to deliver: ...]` envelope. |
| `cron__create` | `schedule: Option<string>` (5-field cron) · `run_once_at: Option<string>` (RFC3339) · `prompt: string` · `description: Option<string>` (short what/why headline) | Register a durable cron for the caller's project (ANY-caller, like `workers__list`). Exactly one of `schedule` / `run_once_at`. Validates + computes `next_fire` in the host's LOCAL timezone, stamps the caller as owner (`team_role`: the lead, or a worker's label), persists to the machine-local store, returns the cron id. The optional `description` (trimmed; blank → `None`) headlines the row in the SCHEDULES section, falling back to the prompt's first line. On schedule the prompt fires into its owner, waking the project (lead + team together) when asleep; durable across restarts (catch-up-once on boot, marked missed when overdue). See the [SCHEDULES section](./inspector-processes.md). |
| `cron__list` | - | Returns the caller's own crons (a lead sees lead crons, a worker its own) as `{id, project, schedule, prompt, next_fire}`. Any-caller. |
| `cron__delete` | `id: string` | Delete a cron by id, scoped to the caller's own crons (a caller manages only what it created). Any-caller. |

- **code** - `crates/forge-workspace/src/mcp/peers.rs` for the four peer Tool impls + `build_server` factory · `crates/forge-workspace/src/mcp/peers/facade.rs` for the `WorkspaceFacade` trait · `crates/forge-workspace/src/mcp/cron.rs` for the three cron Tool impls + `crates/forge-workspace/src/mcp/cron/facade.rs` (`CronFacade`) + `crates/forge-workspace/src/mcp/cron/schedule.rs` (parsing / next-fire) · all combined into one `forge` server by `mcp::build_forge_server`, attached per-session via `crates/forge-agent/src/forge_sdk_worker.rs::build_options_with_callback`
- **auto-approve** - `crates/forge-sdk/src/control_dispatch.rs` fast-paths `tool_name.starts_with("mcp__forge__")` to `PermissionDecision::allow()` before the standard permission policy
