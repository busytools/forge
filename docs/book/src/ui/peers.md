# Peer MCP - cross-agent coordination

Every spawned `claude` child gets an in-process MCP server exposing the peer and cron tools (the table below). The `forge.toml` project name is the agent identity - one session per project. When the LLM in project A calls `peers__ask_agent`, forge wraps the prompt in a bracket-prefixed envelope and dispatches it as a synthetic user turn to project B; B's reply lands as another wrapped envelope on A's chat. The renderer matches the wrappers and shows a styled peer block instead of the raw bracket prose. All `mcp__forge__*` calls are auto-approved, and the default tool card is suppressed so the chat shows the styled block. See the [Projects pane](./projects-pane.md) for the per-row in-flight badges.

## Peer / worker chat blocks

Every peer / worker tool call and inbound envelope renders as one block shape: a TitleCase verb names the kind, a directional icon (`⤴` out, `⤵` in) the direction, and the body indents under the tool-card connectors. No source label sits above - the row names its own kind and peer.

| Verb | Direction | Comes from |
|---|---|---|
| `Tell` | outbound unsolicited | `workers__tell` / `peers__tell_agent` |
| `Ask` | outbound question | `workers__ask` / `peers__ask_agent` |
| `Message` | inbound unsolicited | a `[Message ...]` envelope |
| `Question` | inbound question | a `[Question ...]` envelope |
| `Reply` | inbound response | a `[Reply ...]` envelope; a late one carries a `⚠ late` modifier |

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

<details>
<summary>Peer block details</summary>

- Collapse: the body ellipses to one line (`└─ <first 60 chars>...`); click the row to expand the full body inline.
- Same-worker streak: three consecutive envelopes from the same worker stack body lines under one header - no repeated `Message <same-name>` rows; different workers in the same project still get one header each.
- `workers__spawn` / `workers__list` render as standard tool cards; only the ask / tell calls are suppressed.
- Arrival order: an inbound turn appends at the tail in arrival order - never repositioned above the in-flight assistant turn that holds the outbound send. Delivery strips any stranded empty placeholder (so a rapid Gotify flood never leaves a blank bubble between turns), opens a fresh assistant placeholder at the tail so the thinking spinner pins to the bottom above the input, and flips the session to a running state (chat spinner plus a Projects-pane spin). A resumed history opens no live turn.
- Malformed envelopes fall through to the default user-message rendering rather than erroring. A `[Worker ... spawn failed ...]` envelope stays a one-line system notice with no kind icon - a workspace lifecycle event, not agent traffic.

</details>

## Gotify notification chat block

Every matched Gotify notification delivered into a subscribed session echoes into the chat as an external-notification block, ahead of the response it triggers - every notification is its own turn. It is an external event, not agent traffic: a Gotify source label, the `◈` glyph in place of `▶`, and an `app 'X' - priority N` header over the title then message. The priority number renders in warning at or above 5, otherwise dim. The message body is a user turn, so it carries the [gutter rule](./chat.md#user-message) rather than a background band.

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

<details>
<summary>Gotify block details</summary>

The block never merges into a peer messaging group - it keeps its own source label and the `◈` glyph. It appends at the tail in arrival order, opens a fresh assistant placeholder so the thinking spinner pins to the bottom, and flips the session to a running state - the same delivery path the peer block uses. Collapse is the same one-line ellipsis with click to expand.

</details>

## Cron chat block

Every durable cron that fires into its owner echoes into the chat as a cron block, ahead of the response it triggers. It is an internal scheduled event, not typed input: a Cron source label, the `◴` glyph (the same one the [SCHEDULES](./inspector-processes.md) section uses), and the fired prompt under the tree connectors. An overdue fire (forge or the owner was down through the scheduled minute) prefixes the prompt with a plain `[missed cron]` marker. The fired prompt is a user turn, so it carries the [gutter rule](./chat.md#user-message) rather than a background band.

<div class="term">

  <pre class="indent">
   <span class="accent bold">Cron</span>

     <span class="accent bold">&#x25F4;</span> <span class="bold">Cron</span>
     <span class="dim">&#x2514;&#x2500; run the morning market summary</span>

     <span class="accent bold">&#x25F4;</span> <span class="bold">Cron</span>
     <span class="dim">&#x2502;&nbsp;&nbsp;post the EOD report to #trading</span>
     <span class="dim">&#x2514;&#x2500; include the day's realized PnL</span></pre>

</div>

<details>
<summary>Cron block details</summary>

The `[Cron]` wrapper is display-only: it drives the visible block and inherits the delivered-turn spinner, while the subprocess receives the raw prompt - the bracket never reaches the LLM. The block never merges into a peer messaging group, keeps its own source label and the `◴` glyph, appends at the tail in arrival order with the same placeholder and running-state behavior as the peer and Gotify blocks, and collapses the same way.

</details>

## Sidebar peer-activity badges

On every live project row and worker row in the [Projects pane](./projects-pane.md), a badge cluster sits between the row name and the close ` x ` button. Each badge is `·N` plus a glyph; counts of 0 are omitted. Failure badges disappear 60 s after the counter last incremented. Workers carry their own per-session badges - a forge-asks-worker bumps the worker's `incoming`, a worker-asks-sibling bumps the worker's `outgoing`.

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

<details>
<summary>Badge details</summary>

Sleeping projects have no live state to read peer counters from, so their rows carry no badge column and the name gets the full width instead.

</details>

## Tools exposed by the `mcp__forge__` server

Every spawned `claude` child gets these tools, all auto-approved.

<details>
<summary>The peer and cron tools</summary>

| Tool | Inputs | Semantics |
|---|---|---|
| `peers__whoami` | - | The caller's own status: project, org, cwd, model, in-flight counts. |
| `peers__list_agents` | - | Every project in `forge.toml` with its liveness, model, and in-flight counters; the caller's own row included. |
| `peers__tell_agent` | `target` · `message` · `in_reply_to` (optional) | Fire-and-forget; returns a correlation id. A reply to a still-open ask renders as `Reply` and closes it; a wrong target or stale id demotes to `Message`, with a note in the result to retry. |
| `peers__ask_agent` | `target` · `prompt` | Returns a correlation id; the ask goes in-flight and the reply lands as a synthetic user turn. In-flight until a reply lands or the target is lost. An unknown target fails synchronously; async failures deliver a `[Ask ... failed to deliver: ...]` envelope. |
| `cron__create` | `schedule` (5-field cron) or `run_once_at` (RFC3339) · `prompt` · `description` (optional) | Register a durable cron for the caller's project (any caller). Exactly one of schedule / run-once-at; `next_fire` in the host's local timezone; the caller stamped as owner. The description headlines the [SCHEDULES](./inspector-processes.md) row, else the prompt's first line. Fires into its owner, waking the project when asleep; durable across restarts with catch-up-once on boot. |
| `cron__list` | - | The caller's own crons: id, project, schedule, prompt, next fire. |
| `cron__delete` | `id` | Deletes a cron by id, scoped to the caller's own crons. |

</details>
