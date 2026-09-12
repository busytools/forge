# Inspector processes

## PROCESSES

Currently-running work in the active session: the OS process tree under claude is the source of truth for what is alive, wire-tracked tool calls overlay their description when their command substring-matches a process, and the CLI's background-task registry feeds any backgrounded `local_bash` the OS scan missed. Most rows are one line, `<glyph> <headline> · <memory>`; the depth-0 supervisor shows its whole subtree's resident memory, descendants their own.

| Row kind | Headline | Suffix |
|---|---|---|
| Backgrounded Bash | the assistant's description, else the unwrapped inner command | memory, or `· local_bash` when registry-fed with no scanned process |
| Process (unmatched OS row) | the unwrapped inner command for a shell wrapper, else the basenamed cmdline, else the process name | memory |

| Glyph | Meaning | Color |
|---|---|---|
| `▸` | Backgrounded Bash in progress (headline white bold) | rust orange |
| `▸` | Generic process (headline gray) | dim |
| `○` | Pending (about to run / awaiting permission) | dim |

<details>
<summary>Row order, memory, adoption</summary>

Registry-fed `local_bash` rows the OS scan missed lead the section; the OS-walked rows follow in tree order from claude's direct children, siblings sorted into tiers - matched work, then generic processes - with memory descending within each tier and PID as the tie-break. A 50-row cap applies but the pane scrolls (the scrollbar IS the overflow indicator). Memory renders from 36 pane columns up and drops at Medium. The scan also adopts a backgrounded `local_bash` sitting outside claude's descendant tree - `setsid`-detached, or orphaned once its session's claude exits - so it renders with full RAM and process tree like any other process. Backgrounded agents render in SUBAGENTS and workflows in WORKFLOWS - there is no standalone background section.

</details>

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  PROCESSES</span>

  <span class="accent">  ▸</span> <span class="bold">Print marker in background</span> <span class="dim">· local_bash</span>

  <span class="accent">  ▸</span> <span class="bold">Run unit tests</span> <span class="dim">· 1.2 GB</span>
  <span class="dim">  ├─ cargo nextest run · 256 MB</span>
  <span class="dim">  └─ rustc --crate-name forge_tui · 512 MB</span>

  <span class="dim">  ▸</span> <span style="color: gray">npm run build</span> <span class="dim">· 88 MB</span>

</pre>

</div>

## MCP SERVERS

Sourced entirely from the session's MCP snapshot - never the OS walk - so sdk, http, pending and failed servers all render. Rows render connected first by name, then pending, then failed, packed tight.

- **Name line** - the configured name (plugin namespaces stripped) in bold, plus one status glyph: `●` connected green, `◌` pending blue, `✗` failed red.
- **Detail line** (dim) - `<scope> · <state>`: a connected server appends the tool count (`user · 24 tools`, `no tools` when empty), a pending server the word `pending`, a failed server the CLI's failure reason verbatim.
- **Process line** (dim, subprocess-backed servers only) - `<cmd> · <memory> · <pid>`; same 36-column memory threshold as PROCESSES.

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  MCP SERVERS</span>                    <span class="dim">▦</span>

  <span style="color:var(--text); font-weight:700">    forge</span> <span style="color:rgb(130,199,107)">●</span>
  <span class="dim">      └─ sdk · 18 tools</span>
  <span style="color:var(--text); font-weight:700">    context7</span> <span style="color:rgb(130,199,107)">●</span>
  <span class="dim">      ├─ dynamic · 2 tools</span>
  <span class="dim">      └─ npm exec @upstash/… · 46 MB · 47881</span>
  <span style="color:var(--text); font-weight:700">    playwright</span> <span style="color:rgb(97,160,224)">◌</span>
  <span class="dim">      └─ user · pending</span>
  <span style="color:var(--text); font-weight:700">    granite-ops</span> <span style="color:#e06c75">✗</span>
  <span class="dim">      └─ user · connect refused</span>
</pre>

</div>

<details>
<summary>Process-line join and click-through</summary>

Each snapshot server matches at most one OS process by configured launch command and distinguishing args (so `playwright` and `playwright-local` on the same package get their own processes), falling back to package-name conventions, then a sole-leftover elimination join - two unpaired servers or two candidate processes pair nothing, and an alive tool call (a bash running `npm exec <pkg>` is tracked work, not a server) is never claimed. The winner is the process that reaches the others (the `npm exec` parent over its collapsed `node` child); its whole subtree is claimed from the PROCESSES walk so the server's backing tree never renders twice, and a *wrong* text-match claim makes the process disappear from both sections rather than merely mislabel it. A server with no matched process renders without the third line - the normal case for sdk / http servers, not a gap. The detail line's scope label falls back to `sdk` when the config type says sdk, then `session`; NeedsAuth and Disabled servers never appear in the snapshot a disabled server is dropped from the response entirely. Versions are deliberately not shown per-row - the [Extensions page's Mcps tab](./extensions.md) carries them. The whole section is a click-through: header or any row opens `/extensions` with the Mcps tab selected, including the snapshot refresh, with a dim `▦` affordance at the header's right edge.

</details>

## WORKFLOWS

The active session's in-flight Workflow tool calls, each a `◆ <name>` header over its per-phase tree - spinner while a phase runs, `✓` completed, `○` pending, `log()` lines dim under the active phase. A completed entry collapses to a one-line summary (click to re-expand) and persists only while another entry runs; when none remain the section drops out. A resumed session shows no WORKFLOWS section.

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  WORKFLOWS</span>

  <span class="bold">  ◆</span> <span class="bold">minimal-ping</span>
  <span class="dim">  ├ </span><span class="success">✓</span> Ping <span class="dim">· done</span>
  <span class="dim">  │   └ log: "dispatching single structured agent"</span>
  <span class="dim">  └ </span><span class="dim">⠋</span> Synthesize <span class="dim">· in progress</span>
  <span class="dim">      └ log: "schema validated, awaiting model"</span>

  <span class="success">  ✓</span> <span class="dim">◆ big-fan-out · done</span> <span class="dim">[▶ expand]</span>
</pre>

</div>

<details>
<summary>Workflow fallback</summary>

If the script's `meta` block fails to parse (malformed script, missing `export const meta`), the entry renders as a single static "`◆ Workflow · <status>`" line without a phase tree.

</details>

## MONITORS

Monitor's live tail and summary render in chat (see [Chat](./chat.md)); its state feeds the chat block.

## SUBAGENTS

The sole surface for subagent activity: a dispatch and every child tool call are chat-suppressed; this section renders the dispatch the moment it lands plus a live tail of the child calls. Liveness follows the task's real lifecycle, not the turn, auto-clearing once no root is active.

Each entry pairs the root with a live tail of its last 3-4 child tool calls: the header is `<status> ◇ <label>` - spinner while running, `✓` / `✗` on terminal - with the label from the subagent type plus the prompt's first line; each tail row mirrors the standard chat tool-row chrome, and a finished subagent collapses its tail to a `· N tools` summary.

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  SUBAGENTS</span>

  <span class="accent">  ⠋</span> <span class="bold">◇ Explore · map hidden tool calls</span>
  <span class="dim">      ⌕</span> <span class="bold">Grep</span>  <span class="dim">SubagentChild</span>
  <span class="dim">      ⬚</span> <span class="bold">Read</span>  <span class="dim">inspector_pane.rs</span>
  <span class="dim">      ▶</span> <span class="bold">Bash</span>  <span class="dim">git log --oneline -3</span>

  <span class="success">  ✓</span> <span class="bold">◇ code-reviewer · review the diff</span>   <span class="dim">·  12 tools</span>
</pre>

</div>

<details>
<summary>Subagent liveness details</summary>

The durable signal is the session-scoped roster - the CLI's background-task registry intersected with the session's task-id map, surviving turn finalisation - union a sticky per-session backgrounded-root marker set when an agent-kind task first reports backgrounded and cleared only by a terminal event, so a roster frame lagging the spawning turn's Result cannot collapse the exemption ([#790](https://github.com/busytools/forge/issues/790)); a genuinely-running non-backgrounded root counts alive via its own in-flight status. Auto-clear requires terminal status, roster and marker absence, and no open child left ([#808](https://github.com/busytools/forge/issues/808) keeps a resumed agent visible through its live children). A new dispatch repopulates the section on the next render.

</details>

## SCHEDULES

The session's pending time-based schedules: `ScheduleWakeup` wakeups (the /loop re-arm), `CronCreate` jobs, and durable forge crons. A wakeup stays one line: `⏰`, reason, a live `in <countdown>`. A cron takes two lines: a bold headline (the description, else the prompt's first line) over a dim sub-line with the humanized schedule plus a right-justified badge.

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  SCHEDULES</span>

  <span class="accent">  ⏰</span> <span class="bold">watching CI run</span>            <span class="dim">·  in 8m</span>
  <span class="accent">  ◴</span> <span class="bold">Morning summary</span>
    <span class="dim">daily at 09:00              ·  recurring</span>
  <span class="accent">  ◴</span> <span class="bold">Staging deploy</span>
    <span class="dim">today 14:30                 ·  in 2h10m</span>
</pre>

</div>

<details>
<summary>Schedules details</summary>

- The schedule is plain English (`0 9 * * *` → `daily at 09:00`, `*/5 * * * *` → `every 5 minutes`, `0 9 * * 1-5` → `weekdays at 09:00`, `0 0 1 * *` → `monthly on the 1st`); a run-once cron shows local wall-clock time (`today 14:30`, `tomorrow 09:00`, `Jul 25 09:00`); an unrecognised expression falls back to the raw expr, never a bare `* * * * *`. The badge is `recurring` for a repeating cron (its schedule carries the timing, so no countdown) or a live `in <countdown>` for a run-once; the static `one-shot` badge is the last resort when no fire time could be resolved. Only a cron with neither a description nor a prompt collapses to a single line.
- A session sees only the forge crons it created itself: a lead sees its own entries, a worker labelled `steward` only its own - never the lead's or a sibling's (matching what `cron__list` returns and `cron__delete` acts on). The native sources are per-session by construction. The section hides only when both sources are empty.
- Auto-prune: anything past its fire time - a wakeup or one-shot cron - plus 7-day-expired recurring crons drop on the next ~1 s tick. A one-shot resolves its fire time at decode because the CLI auto-deletes a fired one-shot without emitting a `CronDelete` - waiting for that event would strand the row; the result's job id is stamped onto the entry so a later `CronDelete` by job id finds it. One whose expression does not parse is retained rather than expired against a guess. Wakeups re-arm on each /loop turn - a new `ScheduleWakeup` replaces the prior, so at most one wakeup survives per session.
- Resume replay: every native cron replayed by a resume is skipped - the CLI reports every `CronCreate` as session-only whatever the `durable` flag says, so no replayed native cron has a live counterpart. Only forge crons genuinely survive a restart, and those come from the store.
- Native and forge crons render identically - deliberately; the source is not shown (deferred as its own issue) even though the cancel paths differ (`CronDelete` vs the forge MCP delete).
- Colors: header dim bold; `⏰` and `◴` rust orange; headlines white bold; the sub-line and badges dim.

</details>

## GOTIFY

The session's own inbound Gotify subscriptions plus the stream's connection status. Renders only when the session owns at least one subscription; the status rides the header line - `◈` + "connected", or `⚠` + "disconnected" when the stream drops. One entry per subscription: the app list in white bold, wrapped so no name truncates (`any` when the filter is empty), over a dim `priority >=N` line.

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  GOTIFY</span>     <span class="accent">◈</span> <span class="dim">connected</span>

      <span class="bold">Beszel, Host, Backups, Security, Media</span>
        <span class="dim">priority</span> <span>&gt;=5</span>
      <span class="bold">Alerts, Deploys</span>
        <span class="dim">priority any</span>
</pre>

</div>

Same subscriptions with the stream down - the section stays, the status changes:

<div class="term">

<pre class="indent">
  <span class="dim bold">  GOTIFY</span>  <span class="warning">⚠</span> <span class="dim">disconnected</span>

      <span class="bold">Beszel, Host, Backups, Security, Media</span>
        <span class="dim">priority</span> <span>&gt;=5</span>
</pre>

</div>

<details>
<summary>Gotify details</summary>

- Visibility keys on the owned subscriptions alone, never on the stream: a session that subscribed keeps the section when the connection drops, swapping the status; a session with no owned subscription hides the section without consulting the connection at all.
- Own scope: a session sees only what it subscribed itself - a lead its own entries, a worker its own - so no row carries an owner label. The MCP tools match: `gotify__list` returns only the caller's own subscriptions and `gotify__unsubscribe` removes only those. Worker despawn is the deliberate exception - a departing worker's subscriptions are cleared with no caller involved, so teardown never trips over the ownership check.
- Colors: header dim bold; the status word dim; app names white bold; the `priority` caption and `any` dim; the `>=N` floor white.

</details>

## SLACK

The session's own Slack subscriptions, grouped by workspace. Renders only when the session owns at least one subscription, or when boot could not read the durable set - the failure row names that rather than hiding the section. One heading per workspace carries the label and that workspace's pump status, `◈` connected or `⚠` down, and one dim row per subscription sits beneath it: `#name · every message` or `#name · mentions only` for a conversation, `direct messages` for the DM class, `mentions anywhere` for the workspace mention target.

<div class="term">

<pre class="indent">
  <span class="dim bold">  SLACK</span>

      <span class="bold">Trust Machines</span> <span class="accent">◈</span>
        <span class="dim">#ved-test · every message</span>
        <span class="dim">#ved-test1 · every message</span>
</pre>

</div>

Two workspaces, the second pump down, a mentions-only row, and a conversation whose name was never captured:

<div class="term">

<pre class="indent">
  <span class="dim bold">  SLACK</span>

      <span class="bold">Trust Machines</span> <span class="accent">◈</span>
        <span class="dim">#ved-test · mentions only</span>
      <span class="bold">Acme</span> <span class="warning">⚠</span>
        <span class="dim">C0C0T5E6RM1 · every message</span>
</pre>

</div>

<details>
<summary>Slack details</summary>

- Visibility keys on the owned subscriptions alone, never on the pumps: a session that subscribed keeps the section when a pump drops, swapping its workspace glyph; a session with no owned subscription hides the section. Boot failing to read the durable set shows the section with a warning row instead, so the failure is not silent.
- One pump per workspace, so liveness is per workspace and rides the heading rather than a single status on the section header the way GOTIFY's does. Two subscriptions in one workspace render one heading, not two.
- A conversation's name arrives from one of two sources: the sweep's directory walk, which backfills any record that has none, or the search hit's label, captured when a mention pulls the session into a conversation it was not watching. Either way a record can be rendered before it has a name, and an unnamed row renders its raw id unprefixed. It is never dressed as a channel - the id is not a name, and `#C0C0T5E6RM1` would read as a channel that does not exist.
- Colors: header dim bold; the workspace label white bold; the glyph rust orange connected and warning when down; subscription rows dim.

</details>
