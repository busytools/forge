# Inspector processes

## PROCESSES

The **PROCESSES** section surfaces **currently-running** work in the active session, sitting below **MCP SERVERS** with its own DIM `─` separator, and carrying only **non-MCP** processes - every configured MCP server renders in MCP SERVERS above. The primary data source is **path A+**: the OS process tree under the spawned `claude` binary is the source of truth for what's alive; wire-tracked tool calls overlay their description when their `raw_input.command` substring-matches an OS process's cmdline (after the shell wrapper is unwrapped - see below). The scan also **adopts a backgrounded `local_bash` that sits outside claude's descendant tree** - `setsid`-detached, or orphaned to init once its session's claude exits - by matching the session's live `local_bash` commands, so it renders with full RAM + process-tree like any other process instead of the memory-less fallback below. Layered on top, the CLI's authoritative `background_tasks_changed` registry feeds any backgrounded `local_bash` the OS scan still hasn't surfaced - a short-lived task that finished between the ~1 s scans, or one running before the first scan - deduped against the OS rows by the wire command so a scanned bash never doubles. Backgrounded **agents** render once in **SUBAGENTS** and **workflows** in **WORKFLOWS** (each keyed to the same `task_id` the registry carries), so the registry's non-bash kinds are never surfaced here - there is no standalone BACKGROUND section. Row kinds:

- **Backgrounded Bash** - two sources feed this kind. (1) An OS process that matched a wire-tracked `Bash` with `run_in_background: true`: headline = the assistant's `description`, falling back to the unwrapped inner command when there's no description - never the raw `/bin/zsh -c ...` wrapper; suffix = memory. (2) A CLI-registry `local_bash` from `background_tasks_changed` the OS scan hasn't surfaced (finished between scans, or pre-first-scan): headline = the registry `description`, suffix = the `· local_bash` tag (no memory, no scanned process).
- **Process** - OS process with no matching wire tool call. Foreground `Bash` invocations, grandchildren (e.g. `rustc` workers under a `cargo build`), or anything detached from claude's tool registry. Headline = the unwrapped inner command for a shell wrapper, else the cmdline with the executable basenamed (`cargo nextest run`, not `/opt/homebrew/bin/cargo ...`), else the process name. Args are kept verbatim; only the leading exe path is stripped.

Most rows render as a **single line**: `<glyph> <headline> · <memory>`. The raw `/bin/zsh -c source ... eval '<cmd>' < /dev/null` wrapper claude wraps Bash tool calls in is unwrapped to the inner command for both the headline and the wire-match, so a backgrounded `gh run watch <id>` reads as itself, not `zsh`. **Depth-0 supervisor rows show the whole subtree's resident memory**; descendants keep their own RSS. Registry-fed `local_bash` rows the OS scan missed lead the section; the OS-walked rows follow in DFS pre-order from claude's direct children, siblings sorted into tiers - matched work, then generic processes - with memory descending within each tier and PID as the tie-break. A 50-row sanity cap applies but the pane scrolls (the scrollbar IS the overflow indicator). Memory rendering is tier-gated: Wide (≥36 cols of pane width) appends the `· 12 MB` suffix, Medium drops it. A registry-fed `local_bash` row (no scanned process behind it) has no memory, so it shows its `· local_bash` task-type tag in that suffix slot instead. Status glyphs:

- <span class="accent">▸</span> RUST_ORANGE spinner - `InProgress` backgrounded Bash, whether OS-matched or registry-fed (headline white bold)
- <span class="dim">▸</span> DIM spinner - generic `Process` row (unmatched OS process; headline gray)
- <span class="dim">○</span> DIM - `Pending` (about to run / awaiting permission)

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

The **MCP SERVERS** section sits above PROCESSES and is sourced **entirely from the session's MCP snapshot** (`App::mcp().servers`, refreshed by the connect-time + tick-driven `mcp_status` polls), never from the OS process walk. That is what makes every configured server visible: an `sdk` in-process server (forge itself) has no process for a walk to find, an `http` / `sse` server never spawns one, and a `pending` or `failed` server has no handshake either - all four render here. Rows render in order: **connected first by name, then pending, then failed**.

Row anatomy - up to three lines per server, packed tight (no blank between servers); the bold indented name anchors each block:

- **Name line** - the server's configured name (plugin namespaces stripped, so `plugin:context7:context7` renders as `context7`) in **bold default-fg**, the SCHEDULES headline treatment, indented two columns as its own block, + a trailing status glyph: <span style="color:rgb(130,199,107)">●</span> connected (`REVIEW_RESOLVED` green) / <span style="color:rgb(97,160,224)">◌</span> pending (`REVIEW_ADDRESSED` blue) / <span style="color:#e06c75">✗</span> failed (`STATUS_ERROR` red). One glyph and no word, so the name line never wraps at 40 cols.
- **Detail line** (DIM, indented under the name) - `<scope> · <state>`. The scope is the snapshot's own label rendered verbatim (`user`, `project`, `dynamic` for plugin-provided servers), falling back to `sdk` when the config blob's type says sdk, then `session`. A connected server appends the tool count (`user · 24 tools`, `no tools` for an empty list); a pending server appends the word `pending`; a failed server appends the CLI's failure reason verbatim (e.g. `project · SSE error: Non-200 status code (502)`), truncated with the row like every other pane cell. Versions are deliberately not shown per-row - the [/mcp view](./misc-surfaces.md) still carries them.
- **Process line** (DIM, only for subprocess-backed servers) - `<cmd> · <memory> · <pid>`: the command with its exe basenamed, the **subtree** resident memory (the collapsed `node` leaf's RSS stays inside it), and the pid so a long-lived server can be found with `ps`. Memory + pid ride the same ≥36-col threshold as PROCESSES, so at Medium the line reads as the cmdline alone.

The optional process line comes from a join in `crate::app::mcp_servers::collect_mcp_servers`: each snapshot server is matched to at most one OS process by configured launch command + distinguishing args (so `playwright` and `playwright-local` on the same `@playwright/mcp` package get their own processes), falling back to the package-name conventions, and a server whose process shares no text with its config can still be named when it is the **sole leftover on both sides** - the same elimination join the old PROCESSES tier used, with the same decline-rather-than-guess rule: two unpaired servers or two candidate processes pair nothing, and an alive wire tool call (a bash running `npm exec <pkg>` is tracked work, not a server) is never claimed. The winner is the process that reaches the others (the `npm exec` parent over its collapsed `node` child), its whole subtree is claimed from the walk so PROCESSES never shows the server's backing tree a second time, and the claimed pids feed back to `collect_active_processes`. A server with no matched process renders without the third line - that is the normal case for sdk / http servers, not a gap. A *wrong* text-match claim makes the process disappear from both sections (the walk skips the pid, no row carries it) rather than merely mislabel it.

**The whole section is a click-through** to the [/mcp view](./misc-surfaces.md) - the header and every row stamp one hit band for the section's on-screen rect (clipped when it scrolls off either edge), so clicking anywhere on it mounts the same `ActiveView::Mcp` surface the `/mcp` slash command opens, including the snapshot refresh. The header carries a DIM `▦` affordance at its right edge (same placement the GIT header's `🦉` uses).

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

- **code** - `crates/forge-tui/src/app/mcp_servers.rs::collect_mcp_servers` (row model + server→process join + claimed pids) · rendering in `crates/forge-tui/src/ui/inspector_pane.rs::append_mcp_servers_section` · click band in `::render_scrollable_body`, handled in `crates/forge-tui/src/app/events/mouse.rs` (`PaneHitTarget::InspectorMcpOpenStatus` → `config::open_mcp`)
- **color** - section header (`MCP SERVERS`): `DIM` bold, `▦` affordance `DIM` at the right edge · server name: bold default-fg (the SCHEDULES headline treatment) · status glyph: <span style="color:rgb(130,199,107)">green</span> `●` connected, <span style="color:rgb(97,160,224)">blue</span> `◌` pending, <span class="error">red</span> `✗` failed · detail + process lines + tree connectors: `DIM`
- **data source** - `App::mcp().servers` (the per-session MCP snapshot) for the rows themselves; `UiSession.process_snapshot` only for the optional process line. NeedsAuth / Disabled never appear in the `--print` snapshot today (a disabled server is dropped from the response entirely).

## WORKFLOWS

**WORKFLOWS section**: surfaces the in-flight `Workflow` tool calls for the active session, with completed entries persisting only while any other entry is still running. Each Workflow entry shows: `◆ <meta.name>` header + per-phase tree (in-progress phase carries the spinner; completed phases show `✓`; pending phases show `○`; log lines from `log()` calls render DIM under the active phase). On completion the entry collapses to a one-line summary (click to re-expand to the full tree).

**All-completed clear**: the moment no in-flight entries remain, the entire section drops out (returns when the next `Workflow` tool call fires). A workflow rebuilt by the resume walk is seeded terminal, so a resumed session shows no WORKFLOWS section at all - the walk carries no progress or status events that could retire an in-flight seed.

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

- **code** - `crates/forge-tui/src/ui/inspector_pane.rs::append_workflows_section` (new) · state on `UiSession.workflows: Vec<WorkflowEntry>`
- **color** - section header (`WORKFLOWS`): `DIM` bold · in-progress phase glyph (`⠋`): `DIM` (spinner) · completed phase glyph (`✓`): <span class="success">green</span> · pending phase glyph (`○`): `DIM` · phase title in-progress: white bold · phase title completed: `DIM` · phase title pending: `DIM` · log lines: `DIM` · collapsed completed entry: `DIM` with `[▶ expand]` affordance
- **phase events** - `system/workflow_phase` (per-phase transition with `{phase_name, task_id}`) and `system/workflow_log` (`{message, phase_name}`)
- **data source** - `tool_input.script` parsed for `meta` block (simple substring extraction, no JS interpreter) · phase transitions + logs from `system/workflow_*` events · final result + completion from tool_result
- **graceful fallback** - if `meta` parsing fails (malformed script, missing `export const meta`), the WORKFLOWS entry renders as a single static "`◆ Workflow · <status>`" line without a phase tree

## MONITORS

**MONITORS** is no longer an Inspector section. The Monitor live tail and summary render in the chat pane instead - see the [Monitor tool call (chat surface)](./chat.md) below. `UiSession.monitors` still holds the per-monitor state that feeds the chat block.

## SUBAGENTS

**SUBAGENTS section**: the SOLE surface for subagent activity. A `Task` / `Agent` dispatch and every tool call that subagent makes underneath are BOTH chat-suppressed (both `ToolCallScope::SubagentRoot` and `ToolCallScope::SubagentChild` set `hidden: true` at `tool_calls.rs:243`) - the chat stream shows nothing, and the Inspector SUBAGENTS section renders the dispatch the moment it lands plus a live tail of the child tool calls. Liveness follows the task's real lifecycle, not the turn: a backgrounded subagent's sentinel `tool_result` flips its card terminal while it keeps running, and its spawning turn Results before it finishes, so status alone is unreliable and the turn-scoped alive set is wiped underneath it. The durable signal is the session-scoped roster - the `background_tasks` registry (any kind) ∩ the session-scoped `task_id` → `tool_use_id` map - which survives turn finalisation and covers every backgrounded kind (an agent has no OS process to fall back to, so this roster keeps it visible, mirroring how WORKFLOWS survives across turns), union a sticky per-session `backgrounded_roots` marker: set when an agent-kind task first reports backgrounded and cleared only by a terminal event or the roster departure that drops its mapping, so a roster frame lagging the spawning turn's Result cannot collapse the exemption ([#790](https://github.com/busytools/forge/issues/790)); a genuinely-running non-backgrounded root still counts alive via its own in-flight status. Auto-clears once no root is active - terminal status, absent from roster and sticky marker, and no open child left under it. Sits between WORKFLOWS and SCHEDULES with its own DIM `─` separator.

Each entry pairs the subagent root with a live tail of its last 3-4 child tool calls. The root header is `<status_icon> ◇ <label>` (spinner while running, `✓`/`✗` on terminal) with the label derived from the dispatch's `subagent_type` + first line of `prompt`, shortened the same way chat tool titles are. Each tail row mirrors the standard chat tool-row chrome: `<status> <kind_glyph> <kind_label> <title>` via the existing `theme::tool_name_label` + tool-title shortening. A finished subagent collapses its tail to a `· N tools` summary on the header line.

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

- **code** - `crates/forge-tui/src/ui/inspector_pane.rs::append_subagents_section` reads the derived view from `App::subagents_view()` (in `app/state.rs`): scans the active session's messages for `ToolCallInfo` entries whose registered scope is `SubagentRoot` - or that carries no scope at all but is named as the parent by a registered `SubagentChild` scope, the shape a resumed agent's replayed card leaves ([#808](https://github.com/busytools/forge/issues/808)) - groups `SubagentChild { parent_tool_use_id }` entries under their root, returns per-root `{label, status, tail (last N children), total_count}`
- **data source** - state already in `UiSession` - **no new wire surface**. Roots are the `Task` / `Agent` `ToolCallInfo` blocks visible in the chat stream; children are the `hidden: true` entries the dispatcher already records but never renders. The `ToolCallScope` is stamped at tool-use arrival in `tool_calls.rs::scope_for_tool_call` and read back via `App::tool_call_scope(id)`.
- **auto-clear** - derive returns the empty view when no root is active - terminal status (`Completed` / `Failed` / `Killed`), absent from roster and sticky liveness, and no open child under it ([#808](https://github.com/busytools/forge/issues/808) keeps a resumed agent visible through its live children) - so the entire SUBAGENTS section drops out. A new `Task` dispatch repopulates the section on the next render.
- **color** - section header (`SUBAGENTS`): `DIM` bold · root status_icon: `RUST_ORANGE` spinner (in-progress) / <span class="success">green</span> `✓` (completed) / red `✗` (failed/killed) · root kind-icon (`◇`): white bold · root label: white bold · tail kind-glyph + kind-label: white bold · tail title: `DIM` · terminal-root `· N tools` suffix: `DIM`

## SCHEDULES

**SCHEDULES section**: surfaces pending time-based schedules for the active session, from three sources: `ScheduleWakeup` wakeups (the `/loop` dynamic-pacing re-arm) and `CronCreate` jobs (both chat-parsed cloud routines), plus durable **forge crons** (`mcp__forge__cron`, sourced from the workspace). Sits between SUBAGENTS and PROCESSES with its own DIM `─` separator. A **wakeup** stays one line (`⏰` alarm clock + reason + live `in <countdown>`). A **cron** (`◴` circle-with-upper-left-quadrant) reads as **two lines**: a bold headline - the cron's `description` (forge crons only, from `cron__create`), falling back to the prompt's first line, which is what a native `CronCreate` headlines on since it carries a `prompt` but no description - then a dim sub-line with the **humanized schedule** plus a right-justified `· <badge>`. The schedule is plain English (`0 9 * * *` → `daily at 09:00`, `*/5 * * * *` → `every 5 minutes`, `0 9 * * 1-5` → `weekdays at 09:00`, `0 0 1 * *` → `monthly on the 1st`); a run-once cron shows a local wall-clock time (`today 14:30`, `tomorrow 09:00`, `Jul 25 09:00`), and an unrecognised expression falls back to the raw expr, never a bare `* * * * *`. The badge is `recurring` for a repeating cron (its schedule already carries the timing) or a live `in <countdown>` for a run-once cron; the static `one-shot` badge is the last resort for a run-once whose fire time couldn't be resolved at all. Only a cron with neither a description nor a prompt collapses to a single line showing just its humanized schedule + badge.

**Source is not currently distinguished in the row.** A native `CronCreate` and a durable forge cron render identically - and the two take different cancel paths (`CronDelete` vs `mcp__forge__cron__delete`). Deferred deliberately, tracked as its own issue; the `ScheduleEntry` carries no source field yet.

**Own-scope (forge crons only)**: a session sees only the forge crons it created itself. A project lead sees the `team_role: None` entries; a worker labelled `steward` sees only its own, never the lead's or a sibling worker's. This matches what `cron__list` already returns and what `cron__delete` lets a caller act on. The two **native** sources are untouched by this: `ScheduleWakeup` and `CronCreate` entries live on the session's own `UiSession.schedules` bucket and were always per-session, so a session owning no forge cron still renders SCHEDULES for its own wakeup. The section hides entirely only when both sources are empty.

**Auto-prune**: anything with a passed `fire_at` - a wakeup, or a one-shot cron - plus 7-day-expired recurring crons drop on the next `~1s` tick (`App::prune_expired_schedules` from `git_diff::apply_timer_tick`). A one-shot cron gets its `fire_at` resolved at decode from the expression's first match after creation (`forge_workspace::next_fire_after`, the same croner-backed evaluator the durable crons use), because the CLI auto-deletes a fired one-shot **without** emitting a `CronDelete` - waiting for that event would strand the row forever. A one-shot whose expression doesn't parse keeps no fire time and is retained rather than expired against a guess. Wakeups re-arm on each `/loop` turn - a new `ScheduleWakeup` replaces the prior wakeup regardless of `tool_use_id`, so at most one wakeup entry survives in any session at a time.

**Resume-replay**: every native cron replayed by `load_resume_history` is skipped (`App::upsert_cron_from_tool_input` returns early while `replay_in_progress`), matching the wakeup guard beside it. The CLI reports EVERY `CronCreate` as `Session-only (not written to disk, dies when Claude exits)` whatever the requested `durable` flag says, so no replayed native cron has a live counterpart and no `CronDelete` is coming to clear it. Only `mcp__forge__cron` entries genuinely survive a restart, and those come from the store rather than the transcript.

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

- **code** - `crates/forge-tui/src/ui/inspector_pane.rs::append_schedules_section` (+ `append_schedule_row` → `append_schedule_two_line` / `append_schedule_one_line` + `fmt_countdown` + `forge_cron_to_schedule_entry` + `first_line`) · humanizer in `crates/forge-tui/src/ui/schedule_format.rs` (`humanize_cron` for recurring, `humanize_once` for run-once, using `forge_workspace::env::token_usage::system_timezone` for the local zone) · cloud state on `UiSession.schedules: Vec<ScheduleEntry>` populated by `handle_tool_call` arms for `ScheduleWakeup` / `CronCreate` / `CronDelete` · forge crons from the `App.forge_crons: Vec<CronEntry>` snapshot refreshed on the `~1s` tick by `App::refresh_forge_crons` (reads `Workspace::crons_for_project` by the active tab's stamped project NAME on `UiSession.project` - resolved once at Connect via `Workspace::project_name_for_path`, so a worktree worker's bucket carries its parent project and the panel never re-derives the project per-tick from a stale / synthetic / pre-Connect cwd - then retains only the entries whose `team_role` equals `App::active_session_team_role()`)
- **color** - section header (`SCHEDULES`): `DIM` bold · wakeup glyph (`⏰`) + cron glyph (`◴`): <span class="accent">RUST_ORANGE</span> · headline (wakeup reason / cron description / prompt first line): white bold · cron sub-line (humanized schedule): `DIM`, indented under the headline · trailing badge (`in 8m` / `recurring` / `one-shot`): `DIM` right-justified after the pad-spacer
- **data source** - `ScheduleWakeup.delaySeconds` + `reason` drive the wakeup row (`fire_at = now + delaySeconds`) · `CronCreate.cron` + `prompt` + `recurring` drive the native cron row: the expression humanizes at decode into `ScheduleEntry.schedule`, the prompt's first line becomes the headline, and a one-shot additionally resolves its `fire_at` (the result's job id is stamped onto the entry so a later `CronDelete` by job id finds it). `CronCreate.durable` is deliberately NOT read - the CLI ignores it, so it describes only what the model asked for · forge crons (`mcp__forge__cron`) come from `Workspace::crons_for_project(stamped UiSession.project name)` via the `App.forge_crons` snapshot, narrowed to `team_role == App::active_session_team_role()`; `CronEntry.description` headlines the row (prompt first line as fallback) and `CronEntry.next_fire` drives a run-once countdown · the `~1s` tick prunes native entries via `App::prune_expired_schedules(now)` and refreshes the forge-cron snapshot via `App::refresh_forge_crons`
- **scope note** - A **recurring** cron's humanized schedule (`daily at 09:00`) already conveys the timing, so no next-fire countdown is shown. A **run-once** cron of either source carries a concrete fire time, so it shows the absolute time on the sub-line plus a live `in <countdown>` badge; the static `one-shot` badge now appears only when the expression failed to parse and no fire time could be resolved. The humanizer covers the common shapes (daily / every-N / weekdays / weekly / monthly) and falls back to the raw expr for anything exotic. A native cron's `prompt` is usually one unbroken line, so the "first line" headline is the whole prompt and truncates to the pane.
- **owner scope** - The session's own role comes from `Workspace::worker_lookup_for_session` over the live-worker registry - `None` is the project lead, `Some(label)` a worker. Never the sessions catalog: workers are deliberately absent from it, so a catalog read would report every worker as a lead. Resolved on the `~1s` refresh tick, never per frame, so the render path takes no workspace lock. Applies to the forge crons only; the native wakeup / `CronCreate` rows are per-session by construction.

## GOTIFY

**GOTIFY section**: surfaces the **active session's own** inbound Gotify subscriptions (`mcp__forge__gotify`, sourced from the workspace) plus the stream's live connection status. Renders whenever the session owns **at least one subscription**, connected or not; with none the whole section (header and rows) is hidden. When shown it sits between SCHEDULES and PROCESSES with its own DIM `─` separator. The stream status rides the header line itself, right-justified against the pane's 1-col gutter. Then one entry per subscription (subscriptions are never merged): the full app list in white bold, listing every matched app name comma-joined and wrapped to the pane width so no name is truncated (`any` when the filter is empty), and beneath it a DIM `priority >=N` (or `priority any`) line indented one step deeper.

**Own-scope**: a session sees only what it subscribed itself. A project lead sees the `team_role: None` subscriptions it created; a worker labelled `steward` sees only its own, never the lead's or a sibling worker's. Every row on screen therefore shares one owner, so none carries an owner label.

**Visibility keys on owned subscriptions alone, never on the stream.** A session that subscribed to something keeps the section when the connection drops, swapping the status to a <span class="warning">⚠</span> `disconnected`. A session with no owned subscription hides the section without consulting the connection at all.

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

Same subscriptions with the stream down - the section stays, the status changes. This is the Medium-tier (24-col) layout, the tightest the header renders at, pinned literally by `gotify_header_carries_status_and_fits_wide_and_medium`:

<div class="term">

<pre class="indent">
  <span class="dim bold">  GOTIFY</span>  <span class="warning">⚠</span> <span class="dim">disconnected</span>

      <span class="bold">Beszel, Host, Backups, Security, Media</span>
        <span class="dim">priority</span> <span>&gt;=5</span>
</pre>

</div>

- **code** - `crates/forge-tui/src/ui/inspector_pane.rs::append_gotify_section` (+ `gotify_header_line` for the status-carrying header, mirroring `attention_header_line`), gated by `gotify_section_visible(app) = !gotify_subs.is_empty()`, with `gotify_connected` selecting the header status rather than gating anything · snapshot on `App.gotify_subs: Vec<GotifySubscription>` + `App.gotify_connected: bool`, refreshed on the `~1s` tick by `App::refresh_gotify` (reads `Workspace::gotify_subscriptions_for_project` by the active tab's stamped project NAME on `UiSession.project`, then retains only the rows whose `team_role` equals the session's own, like SCHEDULES)
- **color** - section header (`GOTIFY`): `DIM` bold · status glyph, right-justified on the header row: `◈` <span class="accent">RUST_ORANGE</span> when connected, `⚠` <span class="warning">STATUS_WARNING</span> when not · the status word beside it: `DIM` either way · app names: white bold · the `priority` caption and the `any` word: `DIM` · the `>=N` floor: white (brighter than the caption)
- **data source** - `Workspace::gotify_subscriptions_for_project(stamped UiSession.project name)` drives the subscription entries, narrowed to `team_role == App::active_session_team_role()` · `Workspace::gotify_connected()` drives the status glyph + word ONLY (set by the subsystem pump on `Connected` / `Disconnected`); it does not gate the section, which keys on the owned subscription set alone
- **scope note** - The session's own role comes from `Workspace::worker_lookup_for_session` over the live-worker registry - `None` is the project lead, `Some(label)` a worker. Never the sessions catalog: workers are deliberately absent from it, so a catalog read would report every worker as a lead. Resolved on the `~1s` refresh tick, never per frame, so the render path takes no workspace lock. The MCP side matches: `gotify__list` returns only the caller's own subscriptions and `gotify__unsubscribe` removes only what the caller subscribed, the same owner scoping `cron__list` / `cron__delete` apply. Worker despawn is the deliberate exception - `remove_gotify_subscriptions_for_worker` clears a departing worker's subscriptions with no caller involved, so teardown never trips over the ownership check.
