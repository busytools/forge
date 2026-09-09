# Launchpad - project picker

Floor of the UI, and the second of the launchpad's two views - [preflight](./preflight.md) comes first and hands over once everything has resolved. Renders as the entire frame when forge is invoked without a project argv (`forge` by itself) OR when the user runs `/launchpad` mid-session; every `/launchpad` after the first in a run lands straight here. Both side panes hide while launchpad is up; the ANSI Shadow wordmark + version metadata + org-grouped project picker + footer hint own the full window width. `Esc` is a no-op - the launchpad has nothing to dismiss to. `Ctrl+Q` still quits.

Boot rule is argv-only, and now runs behind [preflight](./preflight.md) on both branches: `forge` ⇒ preflight ⇒ this picker, `forge <project-name>` ⇒ preflight ⇒ that project's chat. No "remember last picked" persistence, no `focus = true` override (the field is still parsed for back-compat but no longer affects routing).

## Launchpad full-screen layout

*visible: `ActiveView::Launchpad` after the hand-over*

The identity block (wordmark + version + optional update line + the account chip row when one is showing) and the picker box ride together as one **vertically-centered unit**; the single-row footer hint stays anchored to the terminal's bottom edge. The leftover height above the footer splits roughly symmetrically above the wordmark and below the box, so there's no dead gap between the wordmark and the list. The box grows to use most of the height left once the identity block is accounted for (capped so the block keeps breathing) and windows its rows by a scroll offset that follows the selection, so a project past the fold scrolls into view instead of being clipped; a scrollbar appears on the box's right edge only when the list overflows. On a long list or short screen the block top-aligns and the box scrolls; short lists render short and centered.

<div class="term">

  <pre class="indent">


            <span class="rust-orange bold">███████╗ ██████╗ ██████╗  ██████╗ ███████╗</span>
            <span class="rust-orange bold">██╔════╝██╔═══██╗██╔══██╗██╔════╝ ██╔════╝</span>
            <span class="rust-orange bold">█████╗  ██║   ██║██████╔╝██║  ███╗█████╗  </span>
            <span class="rust-orange bold">██╔══╝  ██║   ██║██╔══██╗██║   ██║██╔══╝  </span>
            <span class="rust-orange bold">██║     ╚██████╔╝██║  ██║╚██████╔╝███████╗</span>
            <span class="rust-orange bold">╚═╝      ╚═════╝ ╚═╝  ╚═╝ ╚═════╝ ╚══════╝</span>

                          <span class="dim">v0.15.1+7d88141</span>
                          <span class="dim">claude 2.1.133</span>

      <span class="dim">────────────────────────────────────────────────────</span>
        <span class="dim">Busytools</span>                                        <span class="rust-orange">▐</span>
      <span class="rust-orange bold">▶</span> <span class="dim">├─</span> <span class="rust-orange">●</span>  <span class="bold">forge       </span> <span class="dim">(personal)        now</span>         <span class="rust-orange">▐</span>
        <span class="dim">├─</span> <span class="rust-orange">⠹</span>  <span class="dim">data-modules   (personal)   spawning</span>       <span class="rust-orange">▐</span>
        <span class="dim">└─</span> <span class="dim">○</span>  web-api        <span class="dim">(personal)         2d</span>       <span class="rust-orange">▐</span>
                                                         <span class="rust-orange">▐</span>
        <span class="dim">Gateway</span>                                          <span class="dim">│</span>
        <span class="dim">└─</span> <span class="dim">○</span>  core-v1        <span class="dim">(gateway)          1w</span>       <span class="dim">│</span>
                                                         <span class="dim">│</span>
        <span class="dim">Stargate</span>                                         <span class="dim">│</span>
        <span class="dim">└─</span> <span class="rust-orange">●</span>  <span class="bold">stargate    </span> <span class="dim">(stargate)        12m</span>         <span class="dim">│</span>
      <span class="dim">────────────────────────────────────────────────────</span>

 <span class="dim">↑↓  navigate     enter  open     ?  help     ctrl+q  quit</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/launchpad.rs` (renderer) + `crates/forge-tui/src/app/launchpad.rs` (state + keyboard handler) · wordmark stored as `const FORGE_WORDMARK: [&str; 6]` (43 cols × 6 rows ANSI Shadow figlet)
- **color** - wordmark: `RUST_ORANGE` + BOLD · version lines / org headers / tree connectors / footer hint / account hints / pending glyphs (`○`) / picker rules: `DIM` · idle `●`: `RUST_ORANGE` · running / spawning spinner: `RUST_ORANGE` · failed `✗`: `STATUS_ERROR` · selection indicator: `RUST_ORANGE` + BOLD `▶` arrow in a reserved 2-cell column at the row's left edge for clickable selections, `DIM` + BOLD `▶` when the selected row is in `Spawning` (signals "selected but not yet actionable") · spawning row name: `DIM` (not BOLD) reinforces "not yet interactive" · update indicator (`↑ vX.Y.Z available`): `RUST_ORANGE` · scrollbar (overflow only): `▐` thumb `RUST_ORANGE`, `│` track `DIM`, in a 1-col gutter at the box's right edge
- **data sources** - Projects via `Workspace::list_projects()` grouped by org (alpha-sorted, alpha-sorted within each org) · per-row lifecycle from `UiSession::lifecycle_state` with fallback to `Sleeping` · last-activity from `ProjectView::sessions[0].last_activity` · account hint from `ProjectView::primary_account_hint()` (first item of the org's `accounts` list, lowercased) · forge version from `FORGE_VERSION_SHORT` · claude version + update flag from `app.cli_version_info` (async npm probe)
- **keys** - <kbd>↑</kbd> <kbd>↓</kbd> / <kbd>k</kbd> <kbd>j</kbd> navigate (the list scrolls to keep the selection visible) · <kbd>Enter</kbd> intent-aware (open / start / blocked / no-op - see "Click intent" below) · <kbd>r</kbd> retry (only when row is Failed) · <kbd>?</kbd> toggle help (≡ `/help`) · <kbd>Esc</kbd> no-op · <kbd>Ctrl+Q</kbd> quit (≡ `/quit`) · <kbd>Cmd+Left</kbd> <kbd>Cmd+Right</kbd> (<kbd>Ctrl+Left</kbd> <kbd>Ctrl+Right</kbd> off macOS) swallowed (no panes to toggle)
- **click intent** - Enter behavior depends on the selected row's lifecycle. Footer hint reflects this label.
    - **Idle / Running / Attention / AuthRequired / LoggedOut** → "`enter  open`" → switch chat view to the session.
    - **Sleeping** → "`enter  start`" → dispatch `Command::SpawnProject` and stay on the launchpad. The row transitions Sleeping → Spawning → Idle; a second Enter once it reaches Idle takes the user into chat. Avoids the chat-view "Connecting to Claude Code..." stub for cold projects.
    - **Spawning** → "`enter  ⏳ spawning...`" → no-op. The row's dimmed name + DIM-coloured arrow when selected signal that the project isn't ready. User waits.
    - **Failed** → "`r  retry`" → no-op on Enter; press <kbd>r</kbd> for the explicit retry path (drops failed bucket + dispatches fresh `SpawnProject`).
- **slash subset** - The launchpad has no input area, so slash commands are reached through the corresponding keybindings: `/help` ≡ <kbd>?</kbd>, `/quit` ≡ <kbd>Ctrl+Q</kbd>. `/config` and `/plugins` live behind a picked project - open one with <kbd>Enter</kbd> first. The candidate-filter logic still recognises the four-command subset for any caller that queries it (kept in sync with the spec for forward compatibility).
- **config** - `[ui] spinner` (one of `braille` / `phase_of_moon` / `ember` / `bars_v` / `star` / `sparkle`, default `braille`) is the active spinner style for *every* animated surface - launchpad rows, chat thinking/working, input, projects pane, inspector - one `SpinnerStyle` source of truth. The legacy `launchpad_spinner` key still parses (serde alias). `/spinner` overrides it live and persists the choice to the machine-local redb store (under forge's app-support dir), layered over this forge.toml default (see [the /spinner picker](./pickers.md)).

  `[ui] fps` (whole number, `30`-`240`, default `120` - an 8.33ms repaint interval) is how often the run loop repaints while a spinner is animating, read once at startup. Everything else that drives a repaint - keystrokes, a settling smooth scroll, arriving session updates - already redraws on the next loop wake and is unaffected. The gate never goes coarser than 30ms, the pulse step described below. Spinner styles coarsen to what repaints allow, though no current style is quick enough to reach that: the quickest is `braille` at 32ms, so even the `30` floor paints every style at its own intent. Out-of-range values clamp with a warning and a non-integer falls back to the default; neither stops forge booting. Above 120 the loop's own 4ms wake tick tightens to half the frame interval, since a wake coarser than that cannot land on a frame boundary; at 120 and below it stays 4ms. A reduced-motion preference keeps its own fixed 120ms cadence regardless - the point of it is fewer frames.

  **The tab title tracks activity, not the turn.** It shows the pulsing glyph whenever anything is happening - this session's turn, a compaction, or live background work in any session - and the idle glyph otherwise, off `App::shows_activity`, which the frame ticker also reads - the render loop wraps it as `is_animating` to add preflight, so the title pulses through a model download too. That means it deliberately diverges from the chat: when a turn ends with a backgrounded subagent still running, the chat goes quiet while the title keeps pulsing. The chat is where you are looking; the title is how you know to look. It is not an inconsistency to fix.

  `fps` controls repaints and nothing else. One surface animates off a separate pinned 30ms step (`App::spinner_frame`) and is deliberately *not* tied to it: the terminal tab-title pulse (two glyphs alternating every ten steps). It is not a spinner and does not scale - driven at 120fps it blinks at 12Hz, which reads as flicker rather than motion. The visible spinner glyphs are separate again, deriving from the spinner epoch at their own `SpinnerStyle::cadence_ms`. No frame rate makes a spinner spin *faster* than its style asks, and none of the accepted rates is slow enough to make one slower - `RepaintCadence::effective_cadence_ms` resolves intent against capability, and every style currently lands on its own intent.

## Lifecycle row variants

*drives glyph + colour + right column per row*

Each picker row's appearance depends on the session's lifecycle state. The glyph and the right-column label change in lockstep so the user can scan the picker for "who's live, who's spawning, who's idle, who failed."

<div class="term">

  <pre class="indent">
        ├─ <span class="rust-orange">●</span>  <span class="bold">forge        </span><span class="dim">(personal)        now</span>   <span class="dim"># Connected (Idle): RUST_ORANGE bullet, bold name, relative-time - clickable</span>
        ├─ <span class="rust-orange">⠹</span>  <span class="dim">data-modules   (personal)   spawning</span>   <span class="dim"># Spawning: spinner frame in RUST_ORANGE, dim name (not bold) signals "not yet clickable"</span>
        ├─ <span class="dim">○  web-api      (personal)         2d</span>   <span class="dim"># Sleeping / AuthRequired / LoggedOut / Attention: dim circle, dim name</span>
        └─ <span style="color:#cf6171">✗</span>  <span class="bold">core-v1      </span><span class="dim">(gateway)       failed</span>
              <span style="color:#cf6171">OAuth token expired; run /login to retry...</span></pre>

</div>

- **Failed row** - The error description renders as a DIM-red sub-row directly beneath the project name column (indented 8 cells from row start), truncated to picker width with `...`. The raw message lives in `UiSession::last_connection_error` (populated by the connection-failed event handler, cleared on a successful reconnect).
- **Retry** - Pressing <kbd>r</kbd> while a Failed row is highlighted drops the failed bucket and dispatches a fresh `Command::SpawnProject`. Footer hint shows `r  retry` whenever the selected row is in Failed lifecycle (replaces the `enter  open` slot, since Enter is a no-op on Failed).
- **Selection indicator** - A `RUST_ORANGE` BOLD `▶` arrow renders in a reserved 2-cell column on the left edge of the highlighted row; the rest of the row keeps its native state colors (state glyph, name, hints all visible). When the selected row is in the `Spawning` lifecycle, the arrow drops to `DIM` so the user can tell the focused row isn't actionable - Enter won't take them anywhere until the spawn completes. The previous full-row band design was dropped because `RUST_ORANGE` on `RUST_ORANGE` made the `●` idle bullet and running/spawning spinner invisible on the selected row.

## Account loading gate (#246)

*launchpad blocks project-row clicks until every account resolves*

Forge boots into a per-account loading state machine, one tokio task per `[[accounts]]` entry in `forge.toml`. Each task drives its account through `Loading` -> `Ready` or `Bailed` via a probe chosen by the account's `provider` (a minimal billed `/v1/messages` call for an Anthropic account's setup token, `/v1/key` for OpenRouter, `{host_root}/api/monitor/usage/quota/limit` for Z.ai). Project rows stay dimmed + unclickable until *every* account has reached a terminal state.

**The boot-time instance of this now happens on [preflight](./preflight.md), not here** - the launchpad can no longer be reached while accounts are still resolving. What remains is the mid-session case, which is live for the whole run: the 60 s usage poll takes an account `Ready → Bailed` on a 401, and takes it `Bailed → Ready` once the credential heals. `all_accounts_loaded()` is false for that window, so the gate fires exactly as it did at boot.

The identity block's chip row **appears only while some account is non-`Ready`**, and hides rather than showing a permanently green line. That is deliberately *wider* than the click gate rather than the same condition: `all_accounts_loaded()` counts `Bailed` as terminal, so a bailed account lifts the gate and leaves the rows clickable while still being worth surfacing. The row covers that as well as the mid-flight window, where the rows really are blocked and this is what says why. The gate is global, so an account no project uses can block the whole picker; without the row that window would be unclickable rows with no explanation and no affected chip.

<div class="term">

  <pre class="indent">
                                  <span class="dim">v0.15.1+7d88141</span>
                                  <span class="dim">claude 2.1.133</span>

          <span style="color:#cfc26b">○</span> <span class="dim">gateway</span>   <span style="color:#cfc26b">○</span> <span class="dim">gateway1</span>   <span style="color:#7eb87a">●</span> <span class="dim">personal</span>   <span style="color:#cf6171">⚠</span> <span class="dim">stargate</span>

      <span class="dim">────────────────────────────────────────────────────────</span>
        <span class="dim">Busytools</span>
        <span class="dim">├─ ●  forge       </span> <span class="dim">(gateway)         now</span>
        <span class="dim">  │   ├─ planner   (gateway1)</span>
        <span class="dim">  │   ├─ implementer (personal)</span>
        <span class="dim">  │   ├─ reviewer  (stargate)</span>
        <span class="dim">  │   ├─ debugger  (gateway)</span>
        <span class="dim">  │   └─ tester    (gateway1)</span>
        <span class="dim">└─ ○  web-api     </span> <span class="dim">(personal)         2d</span>
              <span class="dim">no usable accounts</span>
      <span class="dim">────────────────────────────────────────────────────────</span>

 <span class="dim">↑↓  navigate     enter  ⏳ loading accounts...     ?  help     ctrl+q  quit</span></pre>

</div>

- **state glyphs** - `○` yellow = Loading (probe in flight) · `●` green = Ready (fresh probe; account available for assignment) · `⚠` red = Bailed (probe ended in an auth failure; repair is an env edit plus a restart)
- **click gate** - Every project row downgrades to `Block` click intent while any account is non-terminal. Footer hint reads `⏳ loading accounts...` instead of the per-row spawn label. Once every account reaches Ready or Bailed, the gate lifts; rows un-dim and Enter resumes normal behavior. Reachable only mid-session now, via the recovery path above - at boot preflight holds the screen instead.
- **per-project pool** - A project's pool intersects its `[[orgs]].accounts` allow-list with the set of `Ready` accounts, then prefers the accounts not at the usage cap - a session lands on a saturated (100%) account only when every candidate is capped, so an all-exhausted org still spawns rather than going dark. Projects whose pool resolves to empty (every allowed account ended in `Bailed`) render a DIM `no usable accounts` sub-row and stay unclickable even after the gate lifts.
- **per-row account chip** - Each project row carries a trailing `(<account>)` chip showing the lead session's assigned account from the AssignmentPlan; one worker row nests under the project per persisted dynamic worker, each carrying the same four elements the project row does - lifecycle glyph, name, its own chip, and the right-aligned activity column. A worker row's chip and its glyph are independent. The chip is the label's AssignmentPlan entry, which a worker gets when it spawns, so a label that has not spawned this boot renders no chip at all. The glyph is the row's lifecycle, so a persisted row with no live worker shows `○` whether or not it carries a chip. The activity column reads <code>&mdash;</code> for any worker that is neither spawning nor failed, because no per-worker activity timestamp exists. Chip color tracks the assigned account's runtime state: DIM = Normal, STATUS_WARNING = at the usage cap (any window - 5h or weekly; the session still spawns but will throttle, and when every account is capped it was assigned only because nothing else was free), STATUS_ERROR + `⚠` = account flipped to Bailed mid-session. Worker rows are info-only on the launchpad (selection stays on the project row).
- **code** - `crates/forge-workspace/src/account.rs` (LoadingState enum + AccountStateMap.all_loaded) · `crates/forge-workspace/src/account_loader.rs` (per-account state machine) · `crates/forge-workspace/src/assignment_plan.rs` (deterministic `(project, label) -> account` mapping) · `crates/forge-tui/src/ui/launchpad.rs` (chip row + effective_click_intent gate)
- **recovery** - The 60 s usage poll re-probes Bailed accounts. Once a healed credential probes clean again, the poll transitions the account Bailed -> Ready and recomputes the assignment plan with a frozen overlay - existing sessions keep their boot-time account, only NEW sessions pick up the recovered account.
