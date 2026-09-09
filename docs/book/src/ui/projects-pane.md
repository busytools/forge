# Projects pane

## Projects pane (Wide / Medium / Narrow tiers)

*visible: at all terminal widths - Wide (≥160) renders a 32ch inline pane, Medium (120-159) a 24ch inline pane, Narrow (<120) a single-line top bar with on-demand full-screen overlay*

Left-side pane, 32ch wide at Wide tier (24ch at Medium), docked to the left of the chat frame. Lists projects from `forge.toml`'s `[[orgs.projects]]`, **grouped by org** (the project's `[[orgs]].name`). Each org contributes a `DIM`-bold header row plus a `│` continuation row, then one tree-leaf row per project hanging off that trunk on `├─` / `└─` connectors. Orgs sort alphabetically and so do the projects inside each org; there is no recency sort and **no per-project session drilldown** (a project's only children are its **live workers**). Two blank rows separate one org from the next.

Each project row carries a lifecycle glyph, the project name, and a right-edge column that differs by state: a live session gets an `x` close button, a sleeping one gets its last-activity age (`2h`) instead. The active project's name is `RUST_ORANGE` bold, other live projects are default fg + bold, sleeping projects are `DIM` throughout. Those last 5 columns are a **control gutter reserved on every row whatever its state**, so the click body stops short of them on a sleeping row too.

Project rows are mouse-only; chat input keeps keyboard focus. Click any project row to switch the active session to that project's lead - if the lead is already in-process, the swap is instant; if the project is sleeping (no in-process session yet), the click dispatches `Command::SpawnProject` and a synthetic `__spawn_<project>__` bucket fronts a "Waking..." placeholder welcome until `Connected` arrives and migrates the bucket onto the real session UUID. **The sleeping case lands you in that placeholder rather than leaving you where you were**: the bucket does not exist at click time, so the click records the spawn key it is waiting for and the `Spawning` reducer makes the switch when it appears. That record is per-click, which is what keeps the boot wave out - every `auto_start` project emits `Spawning` too, and none of them asked for the tab.

<div class="term">

<pre class="indent">
  <span class="accent-bold">PROJECTS</span>
  <span class="dim">──────────────────────────────</span>

  <span class="dim bold">Gateway</span>
  <span class="dim">│  </span>
  <span class="dim">├─ </span><span class="accent">⠋</span> <span class="accent-bold">gateway-backend</span>       <span class="user-band"> x </span> 
  <span class="dim">│  </span>
  <span class="dim">└─ ○ data-modules</span>          <span class="dim"> 2h</span> 


  <span class="dim bold">Personal</span>
  <span class="dim">│  </span>
  <span class="dim">├─ ○ dotfiles</span>              <span class="dim"> 4h</span> 
  <span class="dim">└─ </span><span class="success">◆</span> <span class="bold">playground</span>            <span class="user-band"> x </span>





  <span class="dim">──────────────────────────────</span>
  <span class="dim">Profile</span>  Stargate
  <span class="dim">Org    </span>  Autonomys
  <span class="dim">ID     </span>  <span class="dim">550e8400</span>         <span class="user-band"> ⧉  </span> 
  <span class="dim">Mode   </span>  <span class="warning">[Auto]</span>
  <span class="dim">Model  </span>  Claude Opus 4.7
  <span class="dim">Effort </span>  Max

  <span class="dim">Ctx</span>  <span class="success">▓▓▓▓▓</span><span class="warning">▓▓</span><span class="dim">░░░░░░░░░░░░</span>   39%
<span class="dim"> 5 compactions</span>                <span class="dim">1M</span>

  <span class="dim">5h </span>  <span class="success">▓▓▓</span><span class="dim">░░░░░░░░░░░░░░░░</span>   15%
                          <span class="dim">1h 48m</span>

  <span class="dim">7d </span>  <span class="success">▓▓▓▓▓</span><span class="warning">▓▓▓▓▓</span><span class="accent">▓▓▓▓▓</span><span class="error">▓▓</span><span class="dim">░░</span>   89%
                           <span class="dim">4d 4h</span>


  <span class="dim">forge  </span>  v1.0.53+3cda0dee
  <span class="dim">claude </span>  v2.1.263   <span class="warning">↑ v2.1.266</span>
</pre>

</div>

The same panel on an **API-billed** account (`provider = "openrouter"`). The `5h` and `7d` groups are replaced by the spend group; everything above and below is unchanged, and the row count is identical so the project list above does not shift when the user switches account. Left, a key with a spending cap; right, one without.

<div style="display: flex; gap: 18px; flex-wrap: wrap;">

<pre>
  <span class="dim">Ctx</span>  <span class="success">▓▓▓▓▓</span><span class="warning">▓▓</span><span class="dim">░░░░░░░░░░░░</span>   39%
<span class="dim"> 5 compactions</span>                <span class="dim">1M</span>

  <span class="dim">day    </span>                  <span class="success">$0.56</span>
  <span class="dim">week   </span>                  <span class="success">$4.10</span>
  <span class="dim">month  </span>                 <span class="success">$12.40</span>
  <span class="dim">balance</span>                 <span class="success">$64.40</span>
  <span class="dim">cap</span>  <span class="success">▓▓▓▓▓</span><span class="warning">▓▓▓▓▓</span><span class="accent">▓▓</span><span class="dim">░░░░░░░</span>   62%
<span class="dim">            $7.60 left · monthly</span>

  <span class="dim">forge  </span>  v1.0.53+3cda0dee
</pre>

<pre>
  <span class="dim">Ctx</span>  <span class="success">▓▓▓▓▓</span><span class="warning">▓▓</span><span class="dim">░░░░░░░░░░░░</span>   39%
<span class="dim"> 5 compactions</span>                <span class="dim">1M</span>

  <span class="dim">day    </span>                  <span class="success">$0.56</span>
  <span class="dim">week   </span>                  <span class="success">$4.10</span>
  <span class="dim">month  </span>                 <span class="success">$20.30</span>
  <span class="dim">balance</span>                 <span class="success">$64.40</span>
  <span class="dim">cap</span>                    <span class="dim">not set</span>
<span class="dim">                    no limit set</span>

  <span class="dim">forge  </span>  v1.0.53+3cda0dee
</pre>

</div>

Even though Mode / Model / Ctx flip when the user switches to another session in another project, the panel's shape, position, and labelling stay put. The panel renders, in order:

- **Identity / posture** - `Profile` (active account display name from `forge.toml`'s `[[accounts]]`, via `app.active_account_display_name`), `Org` (organization on the active account, sourced from `AccountInfo.organization` - dim `-` placeholder until the SDK reports one), `ID` (first 8 chars of the active `session_id` with a trailing `⧉` glyph at the right gutter - click the glyph to copy the full session id to the OS clipboard via `arboard`), `Mode` (permission mode badge: `auto` / `acceptEdits` yellow, `plan` blue, `bypassPermissions` / `dontAsk` red, `default` dim), `Model` (the SDK's `display_name_long` with any `(... context)` wrapper folded inline, e.g. `Claude Opus 4.7` or `Sonnet (200K context)` → `Sonnet 200K`), `Effort` (always shown on its own row).
- **Usage bars** - three filling bars: `Ctx` (per-session context %), `5h` (5-hour plan window), `7d` (combined 7-day window). `Ctx` renders the same on both. Each stretches to the available content width rather than a fixed cell count (`bar_cells_for`: pane width minus the row's fixed chrome, floored at 6 - so 19 cells in the 32ch Wide pane, 11 at Medium). The fill colour is a **per-cell position gradient**: the cells split into four zones - green, yellow, orange, red, left to right, with any remainder going to the leftmost zones - so the rightmost filled cell tells you which zone the bar is in. Empty cells stay `░` in DIM. The `Ctx` bar is followed by a DIM, right-justified token-count on the next line (e.g. `1M` / `200K`) - the model's raw context-window size from `ContextUsageResponse.raw_max_tokens`. That same row carries a DIM **compaction count** on its left (`5 compactions`, singular at `1 compaction`), sharing the row. It is **hidden entirely at zero**. The count is seeded at connect from the `compact_boundary` rows in the resumed transcript and incremented on each live boundary, so it survives a resume and a forge restart. The `5h` and `7d` rows are followed by a DIM, right-justified duration on the next line (e.g. `1h 48m` / `4d 4h`). A single blank row separates each bar group from the next.
- **Spend** (API-billed accounts only, in place of the `5h` / `7d` groups) - `day` / `week` / `month` / `balance` right-justified against the panel's gutter: the first three carry this key's spend for each calendar period, and `balance` is the OpenRouter account's remaining credit pool (`total_credits - total_usage` from `/v1/credits`, fetched by the same probe as the per-key figures). `balance` is deliberately account-wide - every key on the account draws on the same pool, and at zero inference stops - while the period figures are per-key. The `cap` row keeps a three-character label so its bar is the same cell count as `Ctx` above. A key with a spending cap fills that bar against it and the following row reads `$<n> left · <cadence>`; an **uncapped key has no denominator**, so it renders `not set` with no bar. An expiry displaces the cadence (`expires <when>`) rather than claiming another row. Every figure forge has no reading for renders `$-`, never `$0.00`: before the first probe lands all four money rows show `$-`, the cap row shows <code>&mdash;</code>, and the last row says why (`no probe yet`, or the probe failure). The cap figures ride the same per-key response the period figures come from, so a cap added or removed from the provider's dashboard lands on the next poll without a restart.
- **Versions** - two trailing rows: `forge` (the running binary's `CARGO_PKG_VERSION`) and `claude` (the version reported by `claude --version` on the local machine). When the npm registry probe finds a strictly-newer published version under the `@anthropic-ai/claude-code` `latest` dist-tag, a <span class="warning">yellow `↑ vX.Y.Z`</span> indicator appends to the `claude` row. Probes run once at app startup in parallel via `Workspace::fetch_cli_version_info`; missing values render as `-` so the row count stays constant. The `forge` row carries `+<sha>`, and at pane widths too narrow for the full stamp (Medium, 24 cols) the sha is shortened to fit the panel's right gutter.

The `📁 cwd` and `⎇ branch` rows moved to the right-hand [Inspector pane](./inspector.md)'s `GIT` section.

At Narrow tier (<120 cols), the inline pane is replaced by a single-line top bar showing `▤  <active-project>·<active-session>`. Tap `▤` (or <kbd>Cmd+Left</kbd>; <kbd>Ctrl+Left</kbd> off macOS) to expand the overlay - full-screen org-grouped project list, the same account / status panel docked at the bottom, and a `✕` glyph in the banner to dismiss. Picking a project / worker row in the overlay switches active session AND closes the overlay in one action - anywhere on the row except its right-edge control gutter, which carries the row's `x` button when the session is live and is inert when it is not. <kbd>Esc</kbd> also closes. Aggregate unread badge on `▤` is deferred to a follow-up issue.

<div class="term">

<pre class="indent">
<span class="dim">▤</span>  forge·main
<span class="dim">─────────────────────────────────────────────────</span>
chat continues here...
</pre>

</div>

Once the overlay is open, the body is replaced by the full project list (same org-grouped tree as the inline pane, just full-width - the banner and rule span the whole overlay instead of carrying the pane's 1-col left pad) with the account / status panel docked at the bottom of the overlay:

<div class="term">

<pre class="indent">
<span class="accent-bold">▤ PROJECTS</span>                                      <span class="dim">✕</span>
<span class="dim">─────────────────────────────────────────────────</span>

 <span class="dim bold">Gateway</span>
 <span class="dim">│  </span>
 <span class="dim">├─ </span><span class="accent">⠋</span> <span class="accent-bold">gateway-backend</span>                        <span class="user-band"> x </span> 
 <span class="dim">│  │</span>
 <span class="dim">│  └─ </span><span class="dim">●</span> reviewer                            <span class="user-band"> x </span> 
 <span class="dim">│  </span>
 <span class="dim">└─ ○ data-modules</span>                           <span class="dim"> 2h</span> 


 <span class="dim bold">Personal</span>
 <span class="dim">│  </span>
 <span class="dim">├─ ○ dotfiles</span>                               <span class="dim"> 4h</span> 
 <span class="dim">└─ </span><span class="success">◆</span> <span class="bold">playground</span>                             <span class="user-band"> x </span>
    <span class="dim">│</span>
 <span class="dim">   └─ </span><span class="accent">⠋</span> <span class="accent-bold">gpt-tutor</span>                           <span class="user-band"> x </span> 


<span class="dim">─────────────────────────────────────────────────</span>
<span class="dim">  Profile  </span>Stargate
<span class="dim">  Org      </span>Autonomys
<span class="dim">  ID       </span><span class="dim">550e8400</span>                             <span class="dim"> ⧉ </span>
<span class="dim">  Mode     </span><span class="warning">[Auto]</span>
<span class="dim">  Model    </span>Claude Opus 4.7
<span class="dim">  Effort   </span>Max

<span class="dim">  Ctx   </span><span class="success">▓▓▓</span><span class="warning">▓▓</span><span class="dim">░░░░░░░</span>  39%
<span class="dim"> 5 compactions                               200K</span>

<span class="dim">  5h    </span><span class="success">▓▓</span><span class="dim">░░░░░░░░░░</span>  15%
<span class="dim">                                           1h 48m</span>

<span class="dim">  7d    </span><span class="success">▓▓▓</span><span class="warning">▓▓▓</span><span class="accent">▓▓▓</span><span class="error">▓▓</span><span class="dim">░</span>  89%
<span class="dim">                                            4d 4h</span>


<span class="dim">  forge    </span>v1.0.53+3cda0dee
<span class="dim">  claude   </span>v2.1.263<span class="warning">  ↑ v2.1.266</span>
</pre>

</div>

- **code** - `crates/forge-tui/src/ui/projects_pane.rs::render` (inline) · `::render_overlay` (Narrow overlay) · `::render_account_status_footer` (the bottom panel, called from both render paths) · `crates/forge-tui/src/ui/top_bar.rs::render` (Narrow top bar) · click handling in `crates/forge-tui/src/app/events/mouse.rs` · sleeping-project spawn in `crates/forge-tui/src/app/events/mouse.rs::switch_to_project_lead`
- **color** - banner (`PROJECTS` / `▤ PROJECTS`): `RUST_ORANGE` bold · rule: `DIM` · org header: `DIM` bold · tree connectors (`├─` / `└─`) + trunk (`│`): `DIM` · active project: `RUST_ORANGE` bold · background live project: default fg + bold · sleeping project (glyph `○` + name + last-activity age): `DIM` · close button (` x `): gray bold on `USER_MSG_BG` slate · list scrollbar (overflow only): `▐` thumb `RUST_ORANGE`, no track, painted over the list region's right edge · top-bar `▤` icon: `DIM` when overlay closed, `RUST_ORANGE` bold when open · overlay `✕`: `DIM` · panel labels (`Profile`, `Mode`, `Model`, `Effort`, `Ctx`, `5h`, `7d`, and on an API-billed account `day`, `week`, `month`, `balance`, `cap`): `DIM` · spend amounts: <span class="success">green</span> bold, flat with no threshold colouring, because the amount is money rather than a fraction of a cap · the `cap` bar takes the same position gradient as the other bars · `not set` and the spend secondary row: `DIM` · panel values: default fg unless badge-colored (Mode badge follows the legacy footer mode colors) · usage bar fill: per-cell position gradient - four left-to-right zones, <span class="success">green</span> / <span class="warning">yellow</span> / <span class="accent">orange</span> / <span class="error">red</span>, sized from the bar's cell count; empty cells (`░`) DIM · ETA duration lines: `DIM` (bumped to <span class="warning">STATUS_WARNING</span> for the two statuses needing an auth repair, `⚠ expired` / `unauthorized`)
- **state glyphs** - each project row carries a lead-session activity glyph read from the row's live `UiSession` bucket - `⠋` spinner while the session has an in-progress turn (`Running` / `Spawning`), **or when an otherwise-Idle session has a live backgrounded task** (bash / agent / workflow that outlives its turn, via `UiSession::has_live_background_work` reading the CLI's `background_tasks` registry), so a project keeps spinning while a backgrounded `gh run watch` or peer agent is still working after its turn settles · `△` Attention (pending permission prompt), `✕` in <span class="error">red</span> (a turn on this background session died - see the [NEEDS ATTENTION band](./inspector.md); outranks `△`) and `⚠` AuthRequired keep their own glyph even with a live backgrounded task - the promotion is over the Idle bullet only, so a live task never masks a session that needs the user · `●` idle bullet when the session is alive but settled with no background work · `◆` in <span class="success">COMPLETION green</span> (`REVIEW_RESOLVED`'s value) when the session's last turn completed while it was not the active tab - cleared the moment the user opens that session; informational only, never part of the needs-attention family (spinner / `△` / `⚠` / `✕` all outrank it) · `·` Sleeping / Failed / LoggedOut. The glyph is `RUST_ORANGE` on the focused row, terminal default on background rows. A project with **no** live bucket at all is a different row shape entirely - `○` plus a last-activity age instead of a glyph plus a close button. Worker child rows carry the same activity glyph and likewise spin on their own live background work.
- **click** - project row → `switch_to_project_lead`: in-process lead → instant `switch_active_session`; sleeping lead → `Command::SpawnProject`, with `App::pending_spawn_focus` carrying the switch until the bucket lands. The row's `x` button (plus the separator column to its left; the right pad column is inert) closes that session instead of switching to it; every hit target is bounded on columns as well as rows (`PaneHitTarget::contains`, no y-only lookup), and the row body ends where that button's band begins whether or not the row currently has one, so the rightmost 5 columns of a sleeping row and the last column of a live row are deliberately dead. Closing the session you are *looking at* lands you on the row this pane draws directly under it, or the row above when it was the last one - the same order you see, so orgs alphabetically, projects alphabetically within an org, and each project's lead ahead of its workers. Rows with nothing to focus are passed over: a sleeping project, and a worker still inside its spawning window. Closing a session you are not looking at leaves focus alone. At Narrow tier both also close the overlay; the `▤` top-bar icon toggles overlay open/close, and the overlay `✕` glyph dismisses without switching.
- **toggle** - <kbd>Cmd+Left</kbd> (<kbd>Ctrl+Left</kbd> off macOS) at Wide / Medium tiers hides / restores the inline pane (in-memory only - each launch re-derives the default from the terminal width, visible at Wide and hidden below it). At Narrow tier the same chord toggles the transient overlay flag. <kbd>Esc</kbd> also closes the overlay.
- **scope** - All three tiers ship: Wide (≥160) inline 32ch · Medium (120-159) inline 24ch with truncation · Narrow (<120) top-bar + overlay. Click + toggle behaviour tier-agnostic for the inline pane; Narrow uses overlay + `✕` semantics. Mouse-only interaction (plus <kbd>Cmd+Left</kbd> / <kbd>Ctrl+Left</kbd> and <kbd>Esc</kbd>); chat input always has keyboard focus.
- **layout** - The pane is vertically split into two anchored regions: **top** for the project list, **bottom** for the account / status panel. The panel reserves a fixed 20 rows from the bottom up (`ACCOUNT_PANEL_HEIGHT`: rule + 6 identity rows + 1 blank + 2 (Ctx bar + size) + 1 blank + 2 (5h bar + ETA) + 1 blank + 2 (7d bar + ETA) + 1 balance-or-separator row + 1 blank + 2 version rows). An API-billed account spends that sixth row on the account balance instead - 4 spend/balance money rows + 1 cap bar + 1 secondary, with no blank between them - so the count is identical either way and switching account never shifts the project list above. A `debug_assert` in `build_account_panel_lines` pins it. The project list takes everything above. When the project list overflows its region it scrolls within itself (per-pane offset advanced by the mouse wheel, with a `▐` thumb on the right edge once the list is taller than the region); the panel's rows are never reclaimed. If the project list is short, the unused rows in the list region render blank - the panel does NOT slide upward to close the gap. The panel is skipped entirely when the pane height drops below 24 rows (`ACCOUNT_PANEL_MIN_PANE_HEIGHT`).

## Worker tree-children (Projects pane)

*visible: per project in the **Projects pane** when `Workspace.live_workers[project_key]` is non-empty. Rendered at every tier (Wide / Medium / Narrow overlay)*

When a project's lead has spawned workers via `mcp__forge__workers__spawn(label, charter, kick?, resume_kick?, interactive?, resume_session?)`, the projects pane renders them as a tree-subtree immediately below the lead row, indented one level past it so the worker connectors sit at column 4. Worker rows are flat regardless of who spawned them (a worker spawning a sub-worker still appears as a sibling under the same project). Tree-by-spawner is deferred to v2.

The optional `kick` is the worker's first-turn message: when provided it's delivered the moment the worker connects through a rate-limited dispatcher. WITHOUT a `kick` a spawn sits idle until the lead sends a `workers__tell` - a "begin now" line in the charter does not run on its own. The kick lands as a plain first user turn, stashed on the worker's `WorkerEntry` at spawn and enqueued in `maybe_kick_worker_on_connected`.

<div class="term">

<pre class="indent">
  <span class="accent-bold">PROJECTS</span>
  <span class="dim">──────────────────────────────</span>

  <span class="dim bold">Gateway</span>
  <span class="dim">│  </span>
  <span class="dim">├─ </span><span class="accent">⠋</span> <span class="accent-bold">gateway-backend</span>       <span class="user-band"> x </span> 
  <span class="dim">│  │</span>
  <span class="dim">│  ├─ </span><span class="dim">●</span> reviewer           <span class="user-band"> x </span> 
  <span class="dim">│  │</span>
  <span class="dim">│  └─ </span><span class="dim">⠋</span> migrator           <span class="user-band"> x </span> 
  <span class="dim">│  </span>
  <span class="dim">└─ ○ data-modules</span>          <span class="dim"> 2h</span> 


  <span class="dim bold">Personal</span>
  <span class="dim">│  </span>
  <span class="dim">├─ ○ dotfiles</span>              <span class="dim"> 4h</span> 
  <span class="dim">└─ </span><span class="success">◆</span> <span class="bold">playground</span>            <span class="user-band"> x </span>
     <span class="dim">│</span>
  <span class="dim">   └─ </span><span class="accent">⠋</span> <span class="accent-bold">gpt-tutor</span>          <span class="user-band"> x </span> 
</pre>

</div>

- **tree glyph** - box-drawing chars in `DIM`: `├─` for every entry except the last, `└─` for the last, at column 4. Whole prefix renders `theme::DIM`.
- **trunks + breathing gaps** - a gap row sits above the first worker and between adjacent workers (never after the last one - the project-to-project deadzone or the org break takes its place). Each gap row, and each worker row, re-paints the two trunks: the subtree `│` at column 4 unconditionally, the org `│` at column 1 only while the parent project is **not** last in its org. When the parent is last, the org column renders as three blank cells - present for layout, unpainted. Code: `org_trunk_span(parent_is_last)`, fed the same `is_last` the project row's own connector uses.
- **label** - worker's user-supplied `label` from `workers__spawn` (the same value that becomes the `forge:worker:<label>` JSONL tag). Rendered with default fg for `Running`, `DIM` for `Spawning`, <span class="error">STATUS_ERROR</span> for `Failed`, and `RUST_ORANGE` bold when that worker is the focused session (mirroring the project row). Head-truncated with trailing `...` when overflowing the pane width.
- **state glyph** - same glyph column as the project row, read from the worker's own `UiSession` bucket (`Spawning` until its `Connected` lands, so the column never blanks): `⠋` spinner while a turn is in progress or a backgrounded task is live, `●` settled-idle, `◆` in <span class="success">green</span> when the worker's last turn completed while it was not the active tab, `·` sleeping, `△` in <span class="warning">yellow</span> when a non-focused worker has a prompt waiting, `✕` in <span class="error">red</span> when the spawn failed *or* when a turn on this background worker died (the latter outranks `△`). A `Failed` worker adds one `DIM` diagnostic sub-row directly beneath its row, indented to the label column and truncated to the pane width, carrying `WorkerEntry.diagnostic` (or `spawn failed` when none was recorded).
- **close affordance** - per-row `x` button right-justified at the row's right edge (mirrors the lead-row close button). Click dispatches `Command::CloseWorker { project_key, label }`; workspace removes the entry from `live_workers` and releases the worker's `SessionTask`. Closing the worker you are looking at hands focus back to its spawning lead - the row its subtree hangs off - and falls through to the same adjacent-row rule as a lead close when that lead is not live here. **JSONL on disk is not deleted**. The resulting `SessionUpdate::WorkerStatusChanged { Removed, worktree }` also pushes a system-message toast into the worker's spawning-lead session (its `spawned_by_session_id`); dropped when that lead isn't live in this process. The `x` button leaves the worktree alone, so it sends `WorktreeDisposition::Intact` and the toast reads `Worker <label> closed. Worktree preserved at .claude/worktrees/<label>/` (or the bare `Worker <label> closed.` for a non-git worker). Same toast fires for every worker released by the lead-row cascade. `workers__despawn` is the one close gesture that reports a different disposition.
- **despawn (MCP)** - `workers__despawn(label, force?)` is the lead's programmatic clean-close gesture (lead-only) - the MCP counterpart of the per-row `x` button. It runs the same teardown (kill the subprocess on drop + remove from `live_workers` + expire inflight asks + emit `WorkerStatusChanged { Removed }`) AND, unlike the `x` button, cleans up the worker's git worktree. Its `Removed` event therefore fires *after* the worktree step rather than inside the shared teardown - the disposition it carries (`Removed` vs `RemovalFailed`) is not knowable until git has answered, and the toast states it verbatim: `Worker <label> closed. Worktree removed from .claude/worktrees/<label>/`, or `Worker <label> closed. Worktree removal failed; it is still at .claude/worktrees/<label>/`. The disposition is decided by whether the directory is still on disk, not by git's exit code, because git errors for any path it no longer tracks whether or not the directory survives - so a worktree already deregistered and gone toasts `Removed`, which is true of the disk, while the tool result still carries the `worktree_cleanup_warning`, which is true of git. A **clean** worktree is removed; a **dirty** one (uncommitted/untracked changes OR unpushed commits) **blocks** the despawn - returns `{status:"blocked", reason}` and the worker stays live - unless `force=true`, which tears down and discards (`git worktree remove --force`). Nothing is ever silently discarded. The dirty-check runs BEFORE teardown; the teardown runs before the worktree removal (kill first, then remove); a worktree-cleanup failure surfaces as a `worktree_cleanup_warning` on the `{status:"despawned"}` result and never rolls back the kill. The `worktree-<label>` branch claude created for the worker is **reaped** after a successful removal (while the worktree stands it holds the branch checked out and git refuses the delete), gated on `git rev-list --count refs/heads/worktree-<label> --not --exclude=refs/heads/worktree-<label> --all` being `0` - i.e. deleting the ref would strand no commit, because every commit on it is also reachable from another ref (including a remote-tracking one, so an already-pushed branch counts) or from some worktree's HEAD. That is a reachability question, not a merged-ness one, so a squash merge is irrelevant to it. A branch that *does* carry commits reachable from nothing else is left in place and named in a `branch_cleanup_warning` (with its tip sha and the commands to inspect and delete it); the despawn still succeeds, since the branch is uninspectable while the worktree pins it. Persisted [review threads and reviews](./misc-surfaces.md) for the branch the worktree had *checked out* are dropped only when that branch no longer resolves after the reap - as a local head **or as any remote-tracking ref**, so a branch that was pushed and has an open PR keeps its review state even though the reap deleted the local ref (the reap counts that same remote as reachability, which is exactly why the local ref went). A worker that made its own feature branch likewise leaves it standing and its review state survives the worker. The check runs after the reap rather than before, because a worker that made no branch of its own sits on `worktree-<label>` and the reap is then what orphans the threads. A branch git cannot inspect counts as present, so nothing is discarded on an unreadable repo. Despawn only ever judges the branch it just tore down, though, and the usual shape is a branch that outlives its worker as an open PR and is deleted after the merge - so **a sweep at boot re-checks every branch the store still holds review state for**, against one listing of that project's local heads and remote-tracking refs, and drops the ones with no ref left. It skips a project whose root does not answer as a work-tree root or that is not in `forge.toml`, skips the `worktree-<label>` of any registered worker, and refuses a project outright when most of its stored branches read as dead *and* the repo holds fewer than three branch refs - which is what a shallow or `--single-branch` clone looks like from the inside, and is the one case where git answers correctly about a ref set that is simply missing the branches. The `x` button (`CloseWorker`) is unchanged - it does NOT clean the worktree or touch review state. *Auto-close-when-idle is a deferred follow-on*; despawn is always explicit. Code: `crates/forge-workspace/src/mcp/workers.rs::Despawn` + `Command::DespawnWorker` -> `spawn.rs::handle_despawn_worker` (+ `spawn.rs::reap_worker_branch` for the warning text) + worktree helpers in `forge-agent::env::worktree` (`remove_worktree`, `reap_worktree_branch`).
- **label click** - switches active session to that worker's chat. Routes through `App::switch_to_worker` which resolves the worker's `SessionKey` from `live_workers` and falls through to the standard `switch_active_session`.
- **state source** - `WorkerStatus { label, charter, status: Spawning | Running, session_id, spawned_at, spawned_by_session_id }` from `forge-primitives::workers`. Workspace tracks the live set in `live_workers: Mutex<HashMap<ProjectKey, Vec<WorkerEntry>>>` and emits `SessionUpdate::WorkerStatusChanged { project_key, action: Added | Removed | StatusChanged, status }` on every transition.
- **lifetime** - workers are process-lifetime only. `forge` restart drops the entire `live_workers` map; the projects pane has zero tree-children at launch. Existing JSONLs survive but are excluded from `/resume` by default (catalog scan filters `forge:worker:*` tags unless `include_workers: true` is passed).
- **code** - `crates/forge-tui/src/ui/projects_pane.rs` for tree-glyph rendering + per-row close hit-target stamping (`PaneHitTarget::CloseWorker { project_key, label, y, height, x_start, x_end }`) · click routing in `crates/forge-tui/src/app/events/mouse.rs` · focus switch in `crates/forge-tui/src/app/events/mouse.rs::switch_to_worker`

## Lead row close: worker cascade

*behavior: triggered whenever the lead row's existing `x` button is clicked on a project that has live workers*

The lead row's close button is unchanged in shape: it still dispatches the standard close-session path via `PaneHitTarget::CloseSession`. The cascade lives inside `Workspace::release_session`: when the session being released is a lead AND `live_workers[project_key]` is non-empty, the workspace releases every worker's `SessionTask` sequentially first, then falls through to the existing lead release path. From the user's perspective this is synchronous: the lead does not return to `Sleeping` until every worker is gone.

The cascade is observable as a burst of `SessionUpdate::WorkerStatusChanged { action: Removed }` events (one per worker), followed by the lead's own lifecycle transition. All JSONLs persist on disk.

- **code** - `crates/forge-workspace/src/workspace.rs::release_session` (cascade arm) · existing TUI close-button handler in `crates/forge-tui/src/app/events/mouse.rs` is unchanged
- **ordering** - workers released sequentially (one at a time, not in parallel) so any in-flight `DeliverWorkerPrompt` drains cleanly before the next release begins. Lead's own release happens last.
