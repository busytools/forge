# Projects pane

The left-side pane: every project from `forge.toml`, grouped by org, with the active session's account and usage panel at the bottom.

Wide (160 cols up): 32ch inline pane. Medium (120-159): 24ch, truncated. Narrow (under 120): a top bar plus an on-demand full-screen overlay. Orgs and projects sort alphabetically, two blank rows between orgs; no recency sort, no drilldown - a project's only children are its live workers. Rows are mouse-only.

<div class="term">

<pre class="indent">
  <span class="accent-bold">PROJECTS</span>
  <span class="dim">──────────────────────────────</span>

  <span class="dim bold">Gateway</span>
  <span class="dim">│  </span>
  <span class="dim">├─ </span>⠋ <span class="accent-bold">gateway-backend</span>       <span class="user-band"> x </span> 
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

On an **API-billed** account the `5h` / `7d` groups become the spend group, same row count either way. Left: a capped key; right: uncapped.

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

| Region | Shows | Click |
|---|---|---|
| Org header | Dim bold name and trunk `│` | Nothing |
| Project row | Lifecycle glyph and name; ` x ` when live, last-activity age when sleeping | Switch to its lead - instantly in-process; a sleeping one spawns and lands in a `Waking...` placeholder until connected. Boot-time `auto_start` spawns never steal the tab. The ` x ` closes instead of switching |
| Worker row | Subtree beneath its lead, flat siblings | Switch to that worker; its ` x ` closes the worker, JSONL kept |
| Profile / Org | Account name and organization; dim `-` until the SDK reports one | |
| ID | First 8 chars of the session id | `⧉` copies the full id to the OS clipboard |
| Mode | Permission badge: `auto` / `acceptEdits` yellow, `plan` blue, `bypassPermissions` / `dontAsk` red, `default` dim | |
| Model | Long display name, `(... context)` folded - `Sonnet (200K context)` renders `Sonnet 200K` | |
| Effort | Always on its own row | |
| Ctx / 5h / 7d | Bars for the session's context %, the 5h and 7d windows; they stretch to the content width (19 cells Wide, 11 at Medium). `Ctx` adds the raw context window (`1M` / `200K`) and a compaction count (hidden at zero; survives resume and restart); `5h` / `7d` add the remaining time (`1h 48m` / `4d 4h`) | |
| Spend (API-billed) | `day` / `week` / `month` carry this key's spend; `balance` is the account-wide pool (inference stops at zero); `cap` fills a bar against it, `$<n> left · <cadence>` beneath - or `not set`, no bar. An expiry displaces the cadence with `expires <when>` | |
| List scrollbar `▐` | Overflow only, no track | Mouse wheel scrolls the list |

| Glyph | Meaning | Color |
|---|---|---|
| `⠋` | A turn in progress - or a settled session with live background work, so the row keeps spinning | terminal default on every row; the attention / died / auth glyphs keep their own even with live background work - the promotion is over the idle bullet only |
| `△` | Attention: a pending permission prompt | yellow |
| `✕` | A turn on this session died; outranks `△` | red |
| `⚠` | Auth required | |
| `●` | Alive, settled, no background work | |
| `◆` | Last turn completed while not the active tab; clears when opened; informational - needs-attention glyphs outrank it | completion green |
| `·` | Sleeping / Failed / LoggedOut | dim |
| `○` + age | No live session: age instead of a glyph and close button | dim |

Selection highlights exactly one row: the selected session's own row, and nothing else - selecting a worker leaves its lead row plain. A row's glyph reads only that session's own state, so the same state shows the same glyph whether the row is selected or not; the selected row shows itself through the rust orange bold label.

<details>
<summary>Chrome colors, the versions row, spend fine print, panel layout</summary>

- The running forge version with its build sha renders last (shortened to fit at Medium); the local CLI's version carries a yellow `↑ vX.Y.Z` when npm lists a strictly-newer release; missing values render `-`.
- Org headers, tree connectors `├─` / `└─` / `│`, rules and panel labels render dim; the top-bar `▤` is dim when the overlay is closed and rust orange bold when open; the overlay `✕` is dim; the close button ` x ` is gray bold on the slate background; the selected row's name and the `PROJECTS` banner are rust orange bold - the project name only when its lead is the active session; other live project names default bold; the sleeping project row is dim throughout.
- Spend amounts are green bold and flat - money, not a fraction of a cap. `not set` and the spend secondary row are dim. Duration lines are dim, warning-colored on the two auth-repair states `⚠ expired` / `unauthorized`. Bar fill and the `cap` bar use the position gradient, sized from the bar's cell count - the rightmost filled cell names the zone.
- Money with no reading renders `$-`, never `$0.00`; the cap row shows <code>&mdash;</code> and the last line names why (`no probe yet`, or the failure). Cap changes land on the next poll, without a restart. The gradient's remainder cells go to the leftmost zones; empty cells stay `░` dim. The compaction count is singular at 1 (`1 compaction`).
- The panel's shape, position and labelling stay put across session switches - Mode / Model / Ctx flip with the session, the panel does not move. The panel skips entirely below 24 pane rows; a short project list leaves its unused rows blank rather than letting the panel slide up, and the list region scrolls within itself on overflow.

</details>

| Key | Action |
|---|---|
| <kbd>Cmd+Left</kbd> (<kbd>Ctrl+Left</kbd> off macOS) | Wide / Medium: hide or restore the inline pane (re-derived at each launch); Narrow: toggle the overlay |
| <kbd>Esc</kbd> | Closes the overlay. |

<details>
<summary>Close and focus rules</summary>

- The ` x ` and the separator column to its left close instead of switching; the right pad column is inert. Hit targets are bounded by rows and columns: a sleeping row's rightmost 5 columns and a live row's last column are dead whether or not a button is drawn - the row body ends where the button's band begins.
- Closing the session you are looking at lands you on the row drawn directly beneath it, or the row above when it was the last - the same alphabetical order the pane draws, each lead ahead of its workers. Rows with nothing to focus (a sleeping project, a worker still spawning) are passed over. Closing a session you are not looking at leaves focus alone.
- The lead row's ` x ` on a project with live workers closes every worker first - sequentially, one at a time - then the lead itself. It is synchronous from the user's side, and the lead does not return to Sleeping until every worker is gone. All JSONLs persist on disk.

</details>

Narrow: the top bar shows `▤  <active-project>·<active-session>`; tap `▤` (or <kbd>Cmd+Left</kbd>; <kbd>Ctrl+Left</kbd> off macOS) to expand the full-screen overlay: the same list full-width, the same panel at the bottom, `✕` dismisses, <kbd>Esc</kbd> closes, `▤` toggles. Picking a row switches and closes in one action, anywhere except the right-edge control gutter.

<details>
<summary>Narrow-tier notes</summary>

The control gutter carries the row's `x` when live and is inert otherwise; an aggregate unread badge on `▤` is deferred. The overlay body is the same tree full-width (the banner and rule span the overlay, not the pane's 1-col pad) with the panel docked at the bottom. A live row carries ` x `; a sleeping row shows its last-activity age; the chat input keeps keyboard focus.

</details>

<div class="term">

<pre class="indent">
<span class="dim">▤</span>  forge·main
<span class="dim">─────────────────────────────────────────────────</span>
chat continues here...
</pre>

</div>

The overlay's body is the same tree full-width (banner and rule span the overlay, not the pane's 1-col pad), panel docked at the bottom:

<div class="term">

<pre class="indent">
<span class="accent-bold">▤ PROJECTS</span>                                      <span class="dim">✕</span>
<span class="dim">─────────────────────────────────────────────────</span>

 <span class="dim bold">Gateway</span>
 <span class="dim">│  </span>
 <span class="dim">├─ </span>⠋ <span class="accent-bold">gateway-backend</span>                        <span class="user-band"> x </span> 
 <span class="dim">│  │</span>
 <span class="dim">│  └─ </span><span class="dim">●</span> reviewer                            <span class="user-band"> x </span> 
 <span class="dim">│  </span>
 <span class="dim">└─ ○ data-modules</span>                           <span class="dim"> 2h</span> 


 <span class="dim bold">Personal</span>
 <span class="dim">│  </span>
 <span class="dim">├─ ○ dotfiles</span>                               <span class="dim"> 4h</span> 
 <span class="dim">└─ </span><span class="success">◆</span> <span class="bold">playground</span>                             <span class="user-band"> x </span>
    <span class="dim">│</span>
 <span class="dim">   └─ </span>⠋ <span class="accent-bold">gpt-tutor</span>                           <span class="user-band"> x </span> 


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

<details>
<summary>Worker rows</summary>

Rendered at every tier (Wide / Medium / the Narrow overlay). A project's spawned workers render as a tree-subtree beneath the lead row, connectors at column 4. Worker rows are flat regardless of who spawned them - a worker spawning a sub-worker still appears as a sibling under the same project (grouping by spawner is deferred to v2). A worker without a kick waits for its first message: charter text alone does not start it. A provided kick is delivered the moment the worker connects, through a rate-limited dispatcher, as a plain first user turn.

<div class="term">

<pre class="indent">
  <span class="accent-bold">PROJECTS</span>
  <span class="dim">──────────────────────────────</span>

  <span class="dim bold">Gateway</span>
  <span class="dim">│  </span>
  <span class="dim">├─ </span>⠋ <span class="accent-bold">gateway-backend</span>       <span class="user-band"> x </span> 
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
  <span class="dim">   └─ </span>⠋ <span class="accent-bold">gpt-tutor</span>          <span class="user-band"> x </span> 
</pre>

</div>

- Tree glyphs `├─` (every entry but the last) and `└─` (the last), at column 4, dim; a gap row sits above the first worker and between adjacent workers - never after the last one. The org trunk repaints between worker rows only while the parent project is not last in its org; three blank cells when it is.
- Worker label: default fg while running, dim while spawning, error color when failed, rust orange bold when it is the focused session; head-truncated with a trailing `...` on overflow.
- Glyphs: the same column as the project row - `⠋` while a turn runs or background work is live, `●` settled, `◆` completion green when the last turn completed while not the active tab, `·` sleeping, `△` yellow when the worker has a prompt waiting, `✕` red when the spawn failed or a turn on this worker died (that outranks `△`). The column never blanks while spawning. A failed worker adds one dim diagnostic sub-row beneath its row, indented to the label column and truncated to the pane width, carrying the failure (or `spawn failed` when none was recorded).
- Click a worker row to switch to its chat. Its ` x ` button closes the worker instead - the JSONL on disk is not deleted.
- Closing the worker you are looking at hands focus back to its spawning lead (the row its subtree hangs off), falling through to the same adjacent-row rule as a lead close when that lead is not live. The lead's session receives a toast: `Worker <label> closed. Worktree preserved at .claude/worktrees/<label>/` (or the bare `Worker <label> closed.` for a non-git worker); dropped when that lead is not live in this process. Every worker released by the lead-row cascade gets the same toast.
- Workers are process-lifetime: a forge restart drops them all and the pane launches with no tree-children. Their JSONLs survive but are excluded from `/resume` by default unless workers are explicitly included.

**`workers__despawn` (the lead's clean-close)** - The pane's ` x ` button leaves the worktree and review state alone. `workers__despawn` (lead-only, programmatic) runs the same teardown AND removes the git worktree: a clean one goes; one with uncommitted or untracked changes or unpushed commits blocks the despawn and the worker stays live, unless `force=true` tears it down and discards - nothing is ever silently discarded. A worktree-cleanup failure surfaces as a warning on the result and never rolls back the kill. The toast states the outcome verbatim: `Worker <label> closed. Worktree removed from .claude/worktrees/<label>/`, or `Worker <label> closed. Worktree removal failed; it is still at .claude/worktrees/<label>/`.

After a successful removal the worker's `worktree-<label>` branch is reaped when deleting it would strand no commit - every commit on it must be reachable from some other ref (a remote-tracking ref counts, so a pushed branch is reapable) or from some worktree's HEAD; reachability, not merged-ness, so a squash merge is irrelevant. A branch carrying commits reachable from nothing else stays in place, named in a warning with its tip sha and the commands to inspect and delete it. The disposition a close reports is decided by whether the directory is still on disk, not by git's exit code; a branch git cannot inspect counts as present, so nothing is discarded on an unreadable repo.

Review threads and reviews held for a branch that no longer resolves as any local head or remote-tracking ref are dropped by a sweep at boot rather than by the despawn itself; a branch that still exists as a remote keeps its review state through the reap, and a branch the worker created itself is left standing. The sweep skips shallow or single-branch clones, skips projects whose root does not answer as a work-tree root or that are not in `forge.toml`, skips the `worktree-<label>` of any registered worker, and refuses a project outright when most of its stored branches read as dead and the repo holds fewer than three branch refs. Auto-close-when-idle is a deferred follow-on; despawn is always explicit.

</details>
