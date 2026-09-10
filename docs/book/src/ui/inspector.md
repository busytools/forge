# Inspector

## Inspector pane

The right-side mirror of the [Projects pane](./projects-pane.md): 40ch at Wide (160 cols up), 30ch at Medium (120-159) with truncation, and at Narrow an `Inspector ▦` top-bar icon opening the full-screen overlay. Reads strictly from the active session; switching projects swaps its content alongside. Banner and NEEDS ATTENTION band never scroll; everything from GIT down is a scrollable body (mouse wheel, 3 lines per notch, a `▐` rust-orange scrollbar on overflow), and the offset survives leaving and returning.

## NEEDS ATTENTION

Pinned below the banner, shown only when a background session has a prompt pending, a turn that died, or unread worker answers on its review comments. A dim-bold header carries a right-justified count; each row is a glyph, the white-bold project name, a dim `(role)` for workers, and a dim detail, stalest-first.

<details>
<summary>Band mechanics</summary>

The band caps at 5 rows (a dim `+N more` tail) and pushes GIT down while present; its wait-age formats like the SCHEDULES countdowns (`20s`, `3m`, `1h`), ticking on the same ~1 s timer.

</details>

| Row | Glyph | Detail |
|---|---|---|
| Waiting on the user | `△` yellow | `permission · <Tool> · <age>` or `question · <age>` |
| Turn died | `✕` red | `failed · <classification>[ HTTP <status>] · <age>` (e.g. `failed · server_error HTTP 529 · 3m`), falling back to `connection error` for a turn that died without any retries; the failure wins when both signals are live on one session |
| Review replies waiting | `💬` addressed | `review replies · <N> · <age>` - a worker answered comments on a review this session filed; ranks below the other two, nothing is blocked on it |

<details>
<summary>Auto-continue on a transient server error</summary>

A turn that dies on a 5xx the CLI has already retried and given up on is recoverable - the conversation and every completed tool result are still in history - so forge sends one more user turn asking the model to resume, up to 3 attempts spaced 5s / 20s / 60s. While a continuation is armed the session shows no attention row; once the budget is spent the failure falls through to the red `✕`. The budget resets when a turn completes. Server errors only: a rate limit needs its window to reset, and auth / billing / invalid-request / max-output-tokens are not transient. Each attempt is visible - a Warning system message lands in that session's chat naming the status and attempt number, e.g. `Server error (HTTP 529) ended the turn - forge continued the session automatically (attempt 1/3), asking the model to resume rather than restart.` The continuation is a plain user turn of forge-authored text - never a replay of the user's original prompt - telling the model to pick up exactly where it stopped and not to repeat any step, tool call or side effect that already completed. On the active session, firing also lifts the turn-error input lock.

</details>

Clicking a row switches to that session (dismissing the Narrow overlay). Prompt rows re-derive live and auto-hide the instant the last pending prompt is answered.

<details>
<summary>Failure rows latch</summary>

A failure row is latched rather than re-derived - the turn is over - and clears only when you switch to the session or it starts another turn (yours or forge's auto-continue). Parking at Idle is deliberately not a clear; a cancelled turn records no failure at all.

</details>

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

Nine sections render in order, each hidden when its content set is empty, separated by dim rules: **GIT** (always), **TASKS** (todos or a verification nudge), **WORKFLOWS** (a live or recently completed Workflow call), **SUBAGENTS** (a dispatch in flight), **SCHEDULES** (a wakeup or cron still valid), **GOTIFY** (a `[gotify]` server configured), **SLACK** (the session owns a Slack subscription), **MCP SERVERS** (at least one server), **PROCESSES** (a living process descendant). The chat scrollback no longer surfaces these tool cards - Workflow paints nothing, the others at most a minimal notice; Monitor renders in chat.

## GIT

The focused session's cwd, the branch, an optional `PR #N → closes #M #K` row, and up to two stacked diff layers - `uncommitted` (vs HEAD) and `N commits vs <default>` - each with its own subtitle, right-justified `+A -R` totals, and a tree of the top-N changed files grouped by directory (single-child directory chains fold into one row). Polled every 10 s for the focused session only; a selected worker scans its own worktree path.

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

<details>
<summary>GIT shapes and the PR row</summary>

- No snapshot yet: only the path row; branch and layers arrive with the first poll.
- Not a git repo: the whole section is suppressed - no GIT header at all.
- Clean tree on the default branch: path + branch row only (yellow `HEAD` when detached).
- Uncommitted edits only: layer 1 renders below the branch row. Committed-but-unmerged only: layer 2 (`1 commit vs <default>` singular at one). Both: layer 1 first, layer 2 beneath.
- Scanner unhealthy: a dim warning line "`git scanner unhealthy, see logs`" replaces the layer content, distinct from a legitimate non-repo.
- The `PR #N` row resolves against the branch's newest pushed commit sha, not the branch name, so a worktree whose local branch differs from the PR's head ref still resolves; stacked PRs render the most recently updated. `gh` re-shells out only when that sha moved, the branch changed, or the 5-minute refresh timer (300 s) elapsed; same-branch scans reuse the cached row. Clicking the row opens the PR in the system browser (a failed open surfaces a chat warning). GitHub-only via `gh` - other remotes render no row. The closing-issue list truncates to a trailing `...` when it would overflow. Rows are suppressed when there is no open PR, the branch is the default or detached, nothing of HEAD's ancestry is pushed, or `gh` cannot run - a resolved row survives transient `gh` failures until the next lookup succeeds.
- Poll exclusions: synthetic spawn buckets and pre-connect buckets skip the refresh, and a cwd change invalidates the cached snapshot via a generation bump, so any in-flight scan against the old cwd is dropped.

</details>

The GIT header carries a `🦉` glyph when any diff layer is populated - click it to open the [Diff overlay](./diff.md). A <span class="addressed">💬 N</span> badge sits left of it when a worker has answered [review comments](./diff.md) on this branch; background sessions surface the same signal as a NEEDS ATTENTION row.

<details>
<summary>Review-replies badge</summary>

`N` counts the threads whose latest turn is the worker's and whose state is `Addressed` or `Outdated`, in the same accent as the comment cards. Only a reviewer reply, a `✓ Resolve` or a `↺ Reopen` retires a thread - opening `/diff` or reading a card does not. The count is fed by the worker's turn-end notice, recomputes from the store whenever `/diff` hydrates its threads, and recomputes once per session shortly after boot so a restart leaves the signal dark for no longer than that. The badge hides when the header describes a different branch than the count was recorded against, and renders with or without the `🦉`, so a branch whose diff has since been committed away still surfaces its unread answers.

</details>

### Task glyphs

| Glyph | Meaning | Color |
|---|---|---|
| `✓` | Completed (text dim, crossed out) | green |
| `▸` | In progress (text white bold; uses the active form when present; **wraps** onto indented continuation lines) | rust orange |
| `○` | Pending (text gray) | dim |

Only the in-progress item wraps. Completed and pending items truncate with `...` at the pane's right edge.

<details>
<summary>Verification nudge</summary>

When the CLI flags one, a one-line dim-yellow notice sits between the rule and the `TASKS` header until the next task update clears it:

</details>

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>
  <span class="warning">  ⚠ verify before declaring complete</span>

  <span class="dim bold">  TASKS</span>

  <span class="accent">  ▸</span> <span class="bold">Open PR</span>
</pre>

</div>

No todos and a clean default branch:

<div class="term">

<pre class="indent">
  <span class="accent-bold">INSPECTOR</span>
  <span class="dim">─────────────────────────</span>

  <span class="dim bold">  GIT</span>

  <span class="dim">  ~/Projects/forge</span>
  <span class="dim">  ⎇</span> <span class="dim">main</span>
</pre>

</div>

At Narrow tier the inline pane is replaced by the `Inspector ▦` top-bar icon; the overlay carries a `✕` and <kbd>Esc</kbd> closes it. The Projects and Inspector overlays are mutually exclusive.

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

<details>
<summary>Colors</summary>

- Banner `INSPECTOR` rust orange bold; rules and section headers dim; the NEEDS ATTENTION header dim bold with a dim count; row names white bold, `(role)` and details dim; the band's bottom rule dim.
- GIT: path, `⎇` glyph and tree connectors dim; branch name dim on the default branch, rust orange on a feature branch, yellow `HEAD` when detached; layer subtitles dim; totals `+A` green / `-R` red; per-file `+N` green / `-M` red; `+N more` dim italic; the `PR` label, `→`, `closes` and issue numbers dim with the `#N` PR number rust orange; the truncation ellipsis dim; the `🦉` dim; the `💬 N` badge in the addressed accent.
- PROCESSES: the wire-tracked Bash / Monitor `▸` rust orange with a white bold headline; a generic process `▸` dim with gray headline; `○` pending dim; connectors, detail text and memory suffix dim.
- MCP SERVERS: header dim bold with a dim `▦` affordance at the right edge; server name bold; status glyph `●` green when connected, `◌` blue pending, `✗` red failed; detail and process lines dim.
- The body scrollbar is one `▐` rust-orange thumb (no track) for the whole body, not per section; the top-bar `▦` is dim when the overlay is closed and rust orange bold when open; the overlay `✕` is dim.

</details>

## Keys and clicks

| Key | Action |
|---|---|
| <kbd>Cmd+Right</kbd> (<kbd>Ctrl+Right</kbd> off macOS) | Wide / Medium: hide or restore the inline pane (re-derived from the terminal width at each launch); Narrow: toggle the overlay |
| <kbd>Esc</kbd> | Closes the overlay |

<details>
<summary>Click targets</summary>

The `🦉` opens the [Diff overlay](./diff.md) (present only when a layer is populated). The MCP SERVERS section is a whole-section click-through: header or any row opens the same `/mcp` view the slash command opens. Everything else is read-only - file tree, branch line, PR row, TASKS rows, PROCESSES rows. At Narrow tier the `▦` icon toggles the overlay; `✕` dismisses.

</details>
