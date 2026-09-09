# Inspector

## Inspector pane (Wide / Medium / Narrow tiers)

*visible: at all terminal widths - Wide (≥160) renders a 40ch inline pane on the right of chat, Medium (120-159) a 30ch inline pane, Narrow (<120) an `Inspector ▦` icon on the top bar with on-demand full-screen overlay*

Right-side pane, 40ch wide, docked to the right of the chat frame. Mirror of the [Projects pane](./projects-pane.md) on the left in chrome (banner + rule), Narrow-tier overlay semantics, and in-memory visibility model. The horizontal layout is symmetric: each pane is separated from the chat column by a full-height `│` in `DIM` plus 1 col of padding (`PANE_SEPARATOR_WIDTH` + `CHAT_PADDING`, both sides painted by the same `render_pane_separator`). Content sits 1 col in from the pane edge (`PANE_PAD`), matching the left pane. Reads strictly from the active session - switching projects via the left pane swaps the Inspector's content alongside it.

The **banner** (`INSPECTOR` heading + DIM rule beneath it) is pinned at the top - it never scrolls, and neither does the optional **NEEDS ATTENTION band** just below it. Everything below those (starting with the `GIT` section) is a scrollable body: mouse wheel over the body advances the active session's `inspector_scroll_offset` by 3 lines per notch, and a vertical scrollbar appears on the right edge whenever the body overflows the visible area. The offset lives on `UiSession`, so switching away to another session and returning preserves where you were looking, same shape as chat scrollback. Same scroll behaviour applies at Narrow tier inside the full-screen overlay.

A pinned **NEEDS ATTENTION** band sits directly below the banner rule and above the scrollable body, shown only when a **background** session (any session other than the active one) has a prompt pending at the head of its queue, has a turn that died, or is holding unread worker answers on its review comments. The first two mirror the signals the [Projects pane](./projects-pane.md) uses for its yellow `△` and red `✕`, so those surfaces never disagree; the review-reply row is Inspector-only. The band never scrolls: it stays fixed while GIT and the sections below it move, and it pushes GIT down when present (when nothing needs attention the band is absent and GIT sits directly under the rule, exactly as before). A `DIM`-bold `NEEDS ATTENTION` header - styled like the other section headers (`GIT` / `TASKS` / `SUBAGENTS`) and framed by the same blank-line + rule rhythm - carries a right-justified `DIM` count; each row is a glyph + the white-bold project name + a DIM `(role)` for workers + a DIM detail of the kind / tool / wait-age, sorted **stalest-first** so the most-overdue row sits on top. Three row kinds share the band:

- **Waiting on the user** - yellow `△`, detail `permission · <Tool> · <age>` or `question · <age>`. Derived from the front `PromptState.source`.
- **Turn died** - red `✕`, detail `failed · <classification>[ HTTP <status>] · <age>` (e.g. `failed · server_error HTTP 529 · 3m`). An error is not a request for input, so it gets its own glyph and colour. The classification is the `ApiRetryError` from the last `api_retry` of that turn - the only place the wire says what went wrong - and falls back to `connection error` for a turn that died without any retries. When both signals are live on one session the failure wins: the band emits one row per session, and a prompt whose turn has died can no longer be answered.
- **Review replies waiting** - <span class="addressed">💬</span> in `REVIEW_ADDRESSED`, detail `review replies · <N> · <age>`. A worker answered comments on a review this session filed and nobody has come back to them; same count and colour as the **GIT header badge**, which covers the active session the band deliberately excludes. Ranks below the other two - nothing is blocked on it - so a session that is also waiting on the user shows that instead. Clears when the reviewer replies, resolves or reopens, and when a recompute finds the branch owes nothing - so a branch that was read, resolved and merged away stops reporting. Opening `/diff` alone still does not clear it.

**Auto-continue on a transient server error.** A turn that dies on a 5xx the CLI has already retried and given up on is recoverable - the conversation and every completed tool result are still in history - so forge sends one more user turn asking the model to resume, rather than leaving the session dead. Up to **3** attempts spaced **5s / 20s / 60s**; the CLI's own retries are seconds apart and already failed, so forge waits longer. While a continuation is armed the session shows *no* attention row - it is recovering, not waiting on the user - and once the budget is spent the failure falls through to the red `✕` row above. The budget resets when a turn completes, so a later unrelated outage gets the full three again. Deliberately `ServerError` only: a `RateLimit` needs its window to reset (`maybe_recover_from_rate_limit_lock` owns that), and auth / billing / invalid-request / max-output-tokens are not transient, so continuing them only burns the budget. Each attempt is **visible**: a <span class="warning">Warning</span> system message lands in that session's chat naming the status and the attempt number, e.g. `Server error (HTTP 529) ended the turn - forge continued the session automatically (attempt 1/3), asking the model to resume rather than restart.` The continuation itself is a plain `Command::Prompt` carrying forge-authored text - never a replay of the user's original prompt - which tells the model to pick up exactly where it stopped and not to repeat any step, tool call or side effect that already completed. Same mechanism as the `DYNAMIC_WORKER_RESTART_NOTE` kick a resuming worker gets. If the session is the active one, firing also lifts the turn-error input lock.

The band caps at 5 rows (a dim `+N more` line collapses the tail beyond that) and clamps its own height so the scroll body always keeps a few rows - a burst of waiters can never crowd out GIT. The wait-age formats like the SCHEDULES countdowns (`20s`, `3m`, `1h`) and ticks on the same ~1s timer. Clicking a row switches the active session to that session (dismissing the Narrow-tier overlay), so its prompt or error block lands in the chat. Prompt rows re-derive from live state each frame, so they auto-hide the instant the last pending prompt anywhere is answered. A failure row is **latched** rather than re-derived - the turn is over, so there is no live state left to read - and clears on either of two events: the user **switches to** the session (the chat carries the error block, so the row has served its purpose and must not reappear on switching away), or the session **starts another turn** (whether the user prompted it or **forge auto-continued** it). Parking at `Idle` is deliberately *not* a clear - the turn-error path itself parks the bucket there, so clearing on Idle would erase the row the instant it was set. A cancelled turn records no failure at all.

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>
  <span class="dim bold">  NEEDS ATTENTION</span>              <span class="dim">4</span>

  <span class="error">  ✕</span> <span class="bold">gateway-backend</span>  <span class="dim">failed · server_error HTTP 529 · 3m</span>
  <span class="warning">  △</span> <span class="bold">stargate</span>  <span class="dim">question · 20s</span>
  <span class="warning">  △</span> <span class="bold">core-v1</span> <span class="dim">(steward)</span>  <span class="dim">permission · Bash · 3m</span>
  <span class="addressed">  💬</span> <span class="bold">forge</span>  <span class="dim">review replies · 2 · 8m</span>

  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  GIT</span>
  <span class="dim">  ~/Projects/gateway-backend</span>
</pre>

</div>

The pane carries eight sections: **GIT** (always rendered), **TASKS** (rendered when the focused session has todos or a pending verification nudge), **WORKFLOWS** (rendered when any `Workflow` tool call from this session is live or recently completed), **SUBAGENTS** (rendered when a `Task` / `Agent` subagent dispatch is in flight, surfacing the otherwise-hidden child tool calls), **SCHEDULES** (rendered when any `ScheduleWakeup` wakeup or `CronCreate` job is still valid, or the session owns a durable forge cron), **GOTIFY** (rendered when a `[gotify]` server is configured, showing the session's own inbound subscriptions + stream status), **MCP SERVERS** (rendered when the session's MCP snapshot has at least one server), and **PROCESSES** (rendered when claude's spawned process tree has at least one descendant alive - foreground / backgrounded `Bash`, grandchildren like `rustc` workers - plus any backgrounded `local_bash` from the CLI's `background_tasks_changed` registry the OS scan hasn't surfaced). DIM `─` horizontal rules separate adjacent sections so each surface reads visually distinct. The chat scrollback no longer surfaces `Task*` (formerly `TodoWrite`) / `Workflow` / `ScheduleWakeup` / `CronCreate` / `CronDelete` tool-call cards; `Workflow` paints nothing at all and the others at most a minimal start/end notice, with the live state landing in the corresponding Inspector section. `Monitor` tool calls render their live tail directly in the chat pane (see the [Monitor chat surface](./chat.md)). Sections render in order: GIT → TASKS → WORKFLOWS → SUBAGENTS → SCHEDULES → GOTIFY → MCP SERVERS → PROCESSES; each downstream section is hidden when its content set is empty. PROCESSES is a live monitor sourced from `sysinfo`'s descendant walk - when the last process exits the section disappears. Cron jobs route to the dedicated `SCHEDULES` section (retired from PROCESSES post-Inspector-SCHEDULES); the prior "Monitor row" kind in PROCESSES is retired and Monitor's live state now renders in the chat pane; the prior "MCP server" row kind is likewise retired - MCP servers render in [MCP SERVERS](./inspector-processes.md).

The **GIT** section shows the focused session's cwd, the current branch on its own line, an optional `PR #N → closes #M #K` row when the scanner resolved an open pull request, and up to two stacked diff sub-sections - **layer 1** (`uncommitted`: dirty / staged / unstaged tree vs HEAD) and **layer 2** (`N commits vs <default>`: commits ahead of the resolved default branch). Each sub-section carries its own subtitle row + right-justified `+A -R` totals + box-drawing tree of the top-N changed files grouped by directory. Single-child directory chains fold into one row so paths like `crates/forge-agent/src/env/` render as a single header above their file children. The diff side is polled - `git_diff` shells out to `git --numstat` every 10 s for the focused session only; a 1 s ticker pokes the drain pump, the rule "snapshot is `None` OR age ≥ 10 s → fetch" decides whether to spawn a scan. The PR side rides on the same poll but is keyed on the pushed sha: each scan walks HEAD's first-parent ancestry to the newest commit the remote already tracks and only shells out to `gh` when that sha moved, the branch changed, or the 5-minute refresh timer elapsed - so steady-state scans on an unchanged branch reuse the cached PR / closes data for free.

The section **follows the focused session**, not the lead's. When a worker is selected, the scan runs against the worker's worktree path (`<project_root>/.claude/worktrees/<label>/`) so the user sees the worker's branch + in-progress edits, not the lead's view of `main`. The cwd resolution is handled by `Workspace::git_scan_cwd_for_session` - composing `worker_lookup_for_session` with the same `worker_tag_dir` helper that drives the tag-write path.

For a worker on a topic branch with uncommitted edits, both layers populate together - layer 1 shows the in-progress edits, layer 2 shows the committed-but-unmerged work. For the lead on `main` with a clean tree, neither layer renders and the panel collapses to just the branch row (byte-identical to today's clean-default render). Either layer can render alone when the other is empty.

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  GIT</span>                        <span class="addressed">💬 2</span> <span class="dim">🦉</span>

  <span class="dim">  ~/Projects/forge/.claude/worktrees/implementer</span>
  <span class="dim">  ⎇</span> <span class="accent">implementer/issue-185-inspector-git-follows-focus</span>
  <span class="dim">    PR </span><span class="accent">#188</span><span class="dim"> → closes #185</span>
  <span class="dim">    uncommitted        </span><span class="success">+42</span> <span class="error">-8</span>

  <span class="dim">  crates/forge-tui/src/ui</span>
  <span class="dim">  └─ inspector_pane.rs</span>      <span class="success">+42</span> <span class="error">-8</span>

  <span class="dim">    3 commits vs origin/main</span><span class="success">+1667</span> <span class="error">-1139</span>

  <span class="dim">  crates</span>
  <span class="dim">  ├─ forge-agent/src/env</span>
  <span class="dim">  │  └─ git_diff.rs</span>         <span class="success">+648</span> <span class="error">-559</span>
  <span class="dim">  └─ forge-tui/src</span>
  <span class="dim">     ├─ app/diff_overlay.rs</span> <span class="success">+19</span> <span class="error">-13</span>
  <span class="dim">     └─ ui/inspector_pane.rs</span> <span class="success">+340</span> <span class="error">-200</span>
  <span class="dim italic">  +22 more</span>

  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  TASKS</span>

  <span class="success">  ✓</span> <span class="dim" style="text-decoration: line-through">Read the rate-limit code</span>
  <span class="success">  ✓</span> <span class="dim" style="text-decoration: line-through">Add the soft-wording branch</span>
  <span class="accent">  ▸</span> <span class="bold">Adding tests for the</span>
       <span class="bold">near-threshold branch</span>
  <span class="dim">  ○</span> Run cargo nextest
  <span class="dim">  ○</span> Open PR
</pre>

</div>

GIT renders one of these shapes, all keyed off `UiSession.git_diff_snapshot`:

- **No snapshot yet** (pre-first-scan window) - only the path row renders. Branch + layers are added once the first poll completes.
- **Not a git repo** (`snapshot.repo_gate = RepoGate::NotARepo`) - the whole section is suppressed. The user sees no GIT header at all.
- **Clean tree on default branch** - path + branch row in DIM. Both `worktree` and `branch_ahead` are `None`, no sub-sections render (or yellow `HEAD` for detached HEAD + clean tree).
- **Uncommitted edits only** - layer 1 sub-section renders below the branch row: `uncommitted` subtitle + `+A -R` totals + file tree.
- **Committed but unmerged only** - layer 2 sub-section renders: `N commits vs <default>` subtitle + totals + file tree. Singular form `1 commit vs <default>` when only one commit ahead.
- **Both layers** - layers 1 and 2 stack, layer 1 first (uncommitted is the more-recent surface). Each carries its own subtitle + tree.
- **Scanner unhealthy** (`snapshot.repo_gate = RepoGate::ScannerFailed`) - DIM-warning line "`git scanner unhealthy, see logs (target: agent.env_git)`" replaces the layer content, surfacing the failure separately from a legitimate non-repo.

The GIT section header carries a `🦉` glyph at its right edge whenever the snapshot has at least one layer to show. Clicking it opens the full-screen [Diff overlay](./misc-surfaces.md); the auto-detected target prefers layer 1 (`HEAD`) when both are populated, falling back to the default branch when only layer 2 is set. Hidden when both layers are empty - nothing to expand into. The file trees below the header stay read-only; the only diff entry point on this pane is the header glyph.

A <span class="addressed">💬 N</span> badge sits immediately left of the `🦉` when a worker has answered [review comments](./misc-surfaces.md) on this branch and the reviewer hasn't come back to them: `N` is the count of threads whose latest turn is the worker's and whose state is `Addressed` or `Outdated`, coloured `REVIEW_ADDRESSED` so the badge and the comment cards it points at agree. It is **not** cleared by opening `/diff` or by reading a card - only a reviewer reply, a `✓ Resolve` or a `↺ Reopen` retires a thread from the count, so an unanswered reply can't quietly scroll away. The count is parked on the session bucket (fed by the worker's turn-end notice, recomputed from the store whenever `/diff` hydrates its threads, and recomputed once per session shortly after boot so a restart doesn't leave the signal dark until someone opens `/diff`), so rendering it costs a field read rather than a store query. The badge hides itself once the GIT header describes a different branch than the count was recorded against, since `/diff` would open on that one instead; it renders with or without the `🦉`, so a branch whose diff has since been committed away still surfaces its unread answers. Background sessions get the same signal as a **NEEDS ATTENTION** row instead.

The `PR #N` row is appended directly below the branch row (above any diff layers) whenever an open pull request contains the branch's newest pushed ancestor, resolved by `gh api repos/{owner}/{repo}/commits/<sha>/pulls` against that sha - not by branch name, so a worktree whose local branch differs from the PR's head ref (pushed as `HEAD:<name>` with no upstream) still resolves its PR. When several open PRs contain the same commit (stacked PRs), the most recently updated one wins and renders as the single row. Clicking anywhere on the row opens the PR's URL in the system browser through `Command::OpenUrl` - the shell-out runs in `forge-agent::env`, not the render thread, and the hand pointer is the affordance (stamped like the MCP SERVERS band). A failed open surfaces a system warning in the chat rather than failing silently. The PR metadata side is GitHub-only via `gh`: a non-GitHub remote lands in the not-a-github-repo failure path and renders no PR row. The PR number lights up in <span class="accent">RUST_ORANGE</span> (the headline); the `PR` label, the `→` separator, the `closes` label, and the issue numbers all sit in `DIM` as supporting context. When the closing-issue list would overflow the pane width, fitted issues render in order and the tail collapses to a trailing `...` (or to a bare `→ ...` when not even one issue number fits alongside the chrome). Rows are suppressed entirely when there's no open PR, when the branch is the default branch or detached, when nothing of HEAD's ancestry is pushed, or when the first resolution can't run (`gh` isn't installed / authenticated, or the remote isn't a GitHub repository) - a row that already resolved survives transient `gh` failures until the next lookup succeeds.

When a worker is torn down, a system-message toast lands in the chat of the session that spawned it (its `spawned_by_session_id`), never whatever session happens to be focused. What it says is driven by the `WorktreeDisposition` the `WorkerStatusChanged { Removed }` event carries, since the worktree's fate differs by how the worker ended:

- `Absent` - the worker was spawned outside a git repo, or its spawn failed before it had a worktree (creating the worktree is what failed, or the rollback beat the subprocess): `Worker <label> closed.`
- `Intact` - the `x` button on its [tree-child row](./projects-pane.md), or a cascade triggered by the lead's close, neither of which touches the worktree: `Worker <label> closed. Worktree preserved at .claude/worktrees/<label>/`
- `Removed` - gone from disk by the time `workers__despawn` finished, whether or not the despawn is what removed it: `Worker <label> closed. Worktree removed from .claude/worktrees/<label>/`
- `RemovalFailed` - the despawn tried, git refused, and the directory is still on disk: `Worker <label> closed. Worktree removal failed; it is still at .claude/worktrees/<label>/`

The branch outcome is deliberately absent from the toast: the reaped branch is a different object from the worktree, and the `branch_cleanup_warning` already reaches the lead on the `workers__despawn` tool result. Text in `ui::worker_status::format_close_toast`, routed through `push_system_message_to_session(_, Info, _)`.

Item glyphs and styles:

- <span class="success">✓</span> green - `Completed` (text DIM + crossed-out)
- <span class="accent">▸</span> RUST_ORANGE - `InProgress` (text white bold; uses `active_form` when present, else `content`; **wraps** onto continuation lines indented under the glyph)
- <span class="dim">○</span> DIM - `Pending` (text gray)

Only the in-progress item wraps. Completed and pending items truncate with `...` at the pane's right edge - they're scannable status, not actionable text. At Medium tier (30ch) truncation is routine; at Wide tier (40ch) most items fit.

When `TodoWriteOutputMetadata.verification_nudge_needed` is set, a one-line dim-yellow notice sits between the rule and the `TASKS` header until the next `TodoWrite` clears it:

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>
  <span class="warning">  ⚠ verify before declaring complete</span>

  <span class="dim bold">  TASKS</span>

  <span class="accent">  ▸</span> <span class="bold">Open PR</span>
</pre>

</div>

Empty TASKS state - when the active session has no todos, the GIT section renders without a TASKS separator below it. With a clean default branch + no diff to show:

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  GIT</span>

  <span class="dim">  ~/Projects/forge</span>
  <span class="dim">  ⎇</span> <span class="dim">main</span>
</pre>

</div>

At Narrow tier (<120 cols), the inline pane is replaced by an `Inspector ▦` icon on the right end of the same top bar that hosts the `▤` Projects icon on the left. Tap `Inspector ▦` (or <kbd>Cmd+Right</kbd>; <kbd>Ctrl+Right</kbd> off macOS) to expand the overlay - full-screen rendering of the same content with a `✕` glyph in the banner to dismiss. <kbd>Esc</kbd> also closes. The Projects overlay and the Inspector overlay are mutually exclusive: opening one closes the other.

<div class="term">

<pre class="indent">
<span class="dim">▤</span>  forge·main                                  <span class="dim">▦</span>
<span class="dim">─────────────────────────────────────────────────</span>
chat continues here...
</pre>

</div>

<div class="term">

<pre class="indent">
<span class="accent-bold">INSPECTOR ▦</span>                                       <span class="dim">✕</span>
<span class="dim">─────────────────────────────────────────────────</span>

<span class="dim bold">  GIT</span>                                        <span class="addressed">💬 2</span> <span class="dim">🦉</span>

<span class="dim">  ~/Projects/forge/.claude/worktrees/implementer</span>
<span class="dim">  ⎇</span> <span class="accent">implementer/issue-185-inspector-git-follows-focus</span>
<span class="dim">    PR </span><span class="accent">#188</span><span class="dim"> → closes #185</span>
<span class="dim">    uncommitted                            </span><span class="success">+42</span> <span class="error">-8</span>

<span class="dim">  crates/forge-tui/src/ui</span>
<span class="dim">  └─ inspector_pane.rs</span>                       <span class="success">+42</span> <span class="error">-8</span>

<span class="dim">    3 commits vs origin/main               </span><span class="success">+1667</span> <span class="error">-1139</span>

<span class="dim">  crates</span>
<span class="dim">  ├─ forge-agent/src/env</span>
<span class="dim">  │  └─ git_diff.rs</span>                          <span class="success">+648</span> <span class="error">-559</span>
<span class="dim">  └─ forge-tui/src</span>
<span class="dim">     ├─ app/diff_overlay.rs</span>                  <span class="success">+19</span> <span class="error">-13</span>
<span class="dim">     └─ ui/inspector_pane.rs</span>                <span class="success">+340</span> <span class="error">-200</span>
<span class="dim italic">  +22 more</span>

<span class="dim">─────────────────────────────────────────────────</span>

<span class="dim bold">  TASKS</span>

<span class="success">  ✓</span> <span class="dim" style="text-decoration: line-through">Read the rate-limit code</span>
<span class="success">  ✓</span> <span class="dim" style="text-decoration: line-through">Add the soft-wording branch</span>
<span class="accent">  ▸</span> <span class="bold">Adding tests for the near-threshold branch</span>
<span class="dim">  ○</span> Run cargo nextest
<span class="dim">  ○</span> Open PR
</pre>

</div>

- **code** - `crates/forge-tui/src/ui/inspector_pane.rs::render` (inline) · `::render_overlay` (Narrow overlay) · `crates/forge-tui/src/ui/top_bar.rs::render` (Narrow top bar - both icons) · click handling in `crates/forge-tui/src/app/events/mouse.rs` · git-diff poller in `crates/forge-tui/src/app/git_diff.rs` (1 s ticker + 10 s staleness rule) → `forge_workspace::env::git_diff::scan`, which is a wildcard re-export of `forge_agent::env::git_diff::scan` rather than a `Workspace` method · worktree-aware cwd resolution in `forge_workspace::Workspace::git_scan_cwd_for_session` (composes `worker_lookup_for_session` + `worker_tag_dir`) so worker sessions scan their `.claude/worktrees/<label>/` path, not the project root · worker close-toast pathway in `crates/forge-tui/src/ui/worker_status.rs::format_close_toast` + the push site in `crates/forge-tui/src/app/events/client.rs` (the `WorkerStatusChanged { Removed }` arm; the event itself is emitted by `forge_workspace::spawn::emit_worker_removed`)
- **color** - banner (`INSPECTOR` / `INSPECTOR ▦`): `RUST_ORANGE` bold · banner rule + GIT/TASKS/PROCESSES separator rules: `DIM` · NEEDS ATTENTION header (`NEEDS ATTENTION`): `DIM` bold (matches the GIT / TASKS / SUBAGENTS section headers), count: `DIM` · NEEDS ATTENTION pending-prompt row `△`: <span class="warning">yellow</span>, failed-turn row `✕`: <span class="error">red</span> (STATUS_ERROR), waiting-review-replies row `💬`: <span class="addressed">REVIEW_ADDRESSED</span>, project name: white bold, `(role)`: `DIM`, kind / tool / failure / wait-age detail + band bottom rule: `DIM` · section headers (`GIT`, `TASKS`, `PROCESSES`): `DIM` bold · GIT header `🦉` open-diff affordance: `DIM` (right-justified on the GIT section header; visible only when at least one diff layer is populated) · GIT header `💬 N` waiting-review-replies badge: <span class="addressed">REVIEW_ADDRESSED</span> (immediately left of the `🦉`, matching the `ADDRESSED` comment-card accent; hidden when nothing awaits the reviewer or the header has moved to another branch) · GIT path: `DIM` · GIT branch glyph (`⎇`): `DIM` · GIT branch name: `DIM` on default branch, <span class="accent">RUST_ORANGE</span> on a feature branch, <span class="warning">yellow</span> (`HEAD`) for detached HEAD · GIT layer-1 subtitle (`uncommitted`) and layer-2 subtitle (`N commits vs <default>`): `DIM` · GIT subtitle-row aggregate `+A`: <span class="success">green</span>, `-R`: <span class="error">red</span> (right-justified per layer) · GIT `PR` label + `→` separator + `closes` label + issue numbers: `DIM` · GIT PR number (`#1234`): <span class="accent">RUST_ORANGE</span> (the headline) · GIT PR truncation ellipsis (`...`): `DIM` · GIT tree connectors (`├─` / `└─` / `│`) + directory labels + file leaf labels: `DIM` · GIT per-file `+N`: <span class="success">green</span>, `-M`: <span class="error">red</span> · GIT `+N more` overflow: `DIM` italic · TASKS completed glyph (`✓`) + text: <span class="success">green</span> glyph, `DIM` crossed-out text · TASKS in-progress glyph (`▸`) + text: `RUST_ORANGE` glyph, white bold text · TASKS pending glyph (`○`) + text: `DIM` · verification nudge line: <span class="warning">yellow</span> · PROCESSES wire-tracked Bash/Monitor glyph (`▸`): <span class="accent">RUST_ORANGE</span>, headline white bold · PROCESSES generic Process glyph (`▸`): `DIM`, headline gray · PROCESSES pending glyph (`○`): `DIM` · MCP SERVERS header (`MCP SERVERS`): `DIM` bold with a `DIM` `▦` affordance at the right edge · MCP SERVERS server name: bold default-fg (the SCHEDULES headline treatment) · MCP SERVERS status glyph: <span style="color:rgb(130,199,107)">green</span> (`●` connected), <span style="color:rgb(97,160,224)">blue</span> (`◌` pending), <span class="error">red</span> (`✗` failed) · MCP SERVERS detail/process lines + connectors: `DIM` · PROCESSES `└─` continuation connectors + detail/metadata text + memory suffix: `DIM` · body scrollbar (overflow only): `▐` thumb `RUST_ORANGE`, no track, on the body's right edge - one scrollbar for the whole body, not per section · top-bar `▦` icon: `DIM` when overlay closed, `RUST_ORANGE` bold when open · overlay `✕`: `DIM`
- **data source** - GIT: `UiSession.git_diff_snapshot` - produced by the agent-side scanner (`forge_agent::env::git_diff::scan`) which shells out to `git rev-parse` / `symbolic-ref` / `status --porcelain` / `diff --numstat` / `rev-list --count` (for layer 2's commit count), plus `gh pr list --head <branch> --state open --json number,url,closingIssuesReferences --limit 1` for the PR row (branch-keyed cache: only invoked when the branch name changed from the previous snapshot - same-branch scans reuse cached `pr` / `closes` for free). Invoked as `forge_workspace::env::git_diff::scan(cwd, prev)` from a `spawn_local` task in `app/git_diff.rs`, threading the prior snapshot through for the cache hit. There is no mediating `Workspace` method here: `forge_workspace::env::git_diff` is a wildcard `pub use` of the agent module, so the TUI calls the agent function directly under a workspace path. That is the thin-facade boundary in practice - forge-tui is kept off forge-agent by the dependency graph, not by visibility, and workspace re-exports the agent surface verbatim. The cwd is resolved per-session by `Workspace::git_scan_cwd_for_session`: project leads scan their `cwd_raw` as-is, git-repo workers scan their worktree path (`<cwd_raw>/.claude/worktrees/<label>/`) so the snapshot reflects the worker's branch + edits, not the lead's. Polled every 10 s for the focused session only - synthetic spawn buckets and pre-Connect buckets skip refresh. Cwd changes invalidate the cached snapshot via a generation epoch bump so any in-flight scan against the old cwd is dropped. Snapshot exposes `worktree: Option<GitDiffStats>` (layer 1) and `branch_ahead: Option<GitBranchAhead>` (layer 2) independently; both can be populated together for a worker on a topic branch with in-progress edits. Worker close-toast pathway: the `SessionUpdate::WorkerStatusChanged { action: Removed, status, worktree }` event that fades the projects-pane row also fires a system-message `Info` push into the chat of the session that spawned the worker, formatted by `format_close_toast` from the `worktree` disposition. TASKS: per-focused-session `app.todos()` (the `UiSession.todos` bucket from PR #109). `TodoWriteOutputMetadata.verification_nudge_needed` from the latest `TodoWrite` tool result drives the nudge line. PROCESSES: `crate::app::processes::collect_active_processes(app)` consumes the focused session's `UiSession.process_snapshot` (refreshed every ~1 s by `crate::app::process_scanner` → `forge_workspace::Workspace::scan_processes(claude_pid)` → `forge_agent::env::processes::scan` which walks `sysinfo`'s descendants of the claude PID), then enriches each entry by substring-matching the OS cmdline against wire-tracked alive tool calls' `raw_input.command`. Wire-alive lookup uses the session-scoped roster - the `UiSession.background_tasks` registry (any kind) ∩ the `UiSession.session_task_tool_use_ids` map - so an OS-caught backgrounded `local_bash` stays enriched (`BashBackgrounded` + description) after turn finalisation, plus any foreground `Bash` via its live `InProgress` status (a blocking call fires no `task_started`). Only bash carries a `command` field, so a resolved agent in the set never matches an OS row. This is the same session-roster signal the SUBAGENTS section reads. Matched OS entries inherit the wire description as headline + `BashBackgrounded` kind; unmatched entries fall back to `Process` kind - MCP-server processes are claimed by the **MCP SERVERS** join (`crate::app::mcp_servers::collect_mcp_servers`, which pairs each snapshot server to at most one process by configured command text, package conventions, or the elimination join `forge_agent::env::processes::elect_unmatched_server` for a sole unpaired `Connected` stdio server against a sole interpreter-shaped root) and are skipped by the walk entirely, wherever they sit in the tree. Ahead of the OS walk, `background_bash_rows` feeds the CLI's authoritative `UiSession.background_tasks` registry (from `background_tasks_changed`): a `local_bash` entry whose wire command resolves (via the session-scoped `UiSession.session_task_tool_use_ids` map - populated at `task_started`, surviving turn finalisation, so a bash that outlived its turn is still resolvable) and doesn't substring-match a scanned process becomes a synthetic `BashBackgrounded` row (description headline, `· local_bash` tag, no memory), so a short-lived / just-started / pre-first-scan / turn-outlived backgrounded bash never drops. A task whose command can't be resolved (terminal-cleared from the session map) is skipped - any still-alive process is already an OS row, so skipping avoids a duplicate. These synthetic rows lead the collected list - ahead of the OS-walked rows - so backgrounded bash clusters with the OS-caught bash at the top, which also keeps them ahead of the 50-row cap. Non-bash registry kinds are skipped - agents keyed by `task_id` render in SUBAGENTS, workflows in WORKFLOWS. An unrecognised `task_type` (drift) routes to no section and logs a `background_task_unrouted_kind` warn. Headlines unwrap the `/bin/zsh -c ... eval '<cmd>' < /dev/null` shell wrapper to the inner command (`forge_agent::env::processes::extract_inner_command`, which reverses the wrapper's `'"'"'` single-quote escaping so a single-quote command correlates to its process, and which the wire-matcher also normalizes whitespace against) and basename the executable path (`basename_exe` - first token only, args kept). (`Monitor` + `CronCreate` tool calls used to surface here as wire-overlay rows; both have moved to their own Inspector sections.) Depth-0 supervisor rows roll up the subtree's total resident memory - computed over the unclaimed graph, so a process subtree that moved to MCP SERVERS no longer counts toward its (former) parent - and descendants keep their own. Within the OS walk, siblings sort into tiers - matched work, then generic processes - with resident memory descending inside each tier and PID as the tie-break (the leading registry-fed bash rows sit above the whole walk); a 50-row sanity cap is applied at the collector level. The inspector pane itself scrolls (per-session offset on `UiSession.inspector_scroll_offset`, advanced by mouse wheel over the body) so the section is no longer trimmed to top-N for display. Memory rendering is gated on inspector pane width ≥ 36 cols. Claude PID is plumbed through `forge_sdk::Client::claude_pid` → `AgentHandle::claude_pid` → `Workspace::claude_pid(SessionKey)` at the spawn-time stamp before the subprocess reader consumes the `tokio::process::Child`.
- **click** - GIT section header's `🦉` glyph opens the [Diff overlay](./misc-surfaces.md) for the current snapshot (only present when at least one diff layer is populated). The **MCP SERVERS** section is a whole-section click-through - header or any row opens the same `/mcp` view the slash command opens, via a single hit band stamped for the section's on-screen rect (clipped when it scrolls off either edge). Other body items stay read-only - file tree, branch line, PR row, TASKS rows, PROCESSES rows are all non-clickable. At Narrow tier the `▦` top-bar icon toggles overlay open/close; the overlay `✕` glyph dismisses.
- **toggle** - <kbd>Cmd+Right</kbd> (<kbd>Ctrl+Right</kbd> off macOS) at Wide / Medium tiers hides / restores the inline pane (in-memory only - each launch re-derives the default from the terminal width, visible at Wide and hidden below it). At Narrow tier the same chord toggles the transient overlay flag. <kbd>Esc</kbd> also closes the overlay. Mirror of the left pane's <kbd>Cmd+Left</kbd> / <kbd>Ctrl+Left</kbd>.
- **scope** - All three tiers ship: Wide (≥160) inline 40ch · Medium (120-159) inline 30ch with item truncation · Narrow (<120) top-bar icon + overlay. Three sections live: `GIT` (always - except when the focused session's cwd is a clean non-repo and the scanner is healthy, then the section is suppressed entirely), `TASKS` (when todos / nudge present), `PROCESSES` (when long-running tool calls present). The pane scaffold remains sized to accommodate additional sections (e.g. SUBAGENTS once the multi-agent epic lands) stacking under PROCESSES without restructuring.
