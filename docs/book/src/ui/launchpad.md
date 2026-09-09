# Launchpad - project picker

The floor of the UI, the second of the launchpad's two views - [preflight](./preflight.md) hands over once everything has resolved. Full-frame when forge starts without a project argument and for every later `/launchpad`; both side panes hide. <kbd>Esc</kbd> is a no-op; <kbd>Ctrl+Q</kbd> quits. Boot is argv-only: `forge` → preflight → this picker, `forge <project>` → preflight → that project's chat. No remember-last persistence.

## Launchpad full-screen layout

The identity block (wordmark + version + optional chip row) and the picker ride as one vertically-centered unit, the footer hint anchored to the bottom edge. The box grows capped and scrolls its rows to follow the selection; a scrollbar appears on the box's right edge only when the list overflows. A long list or short screen top-aligns the block; short lists render short and centered.

<div class="term">

  <pre class="indent">


            <span class="rust-orange bold">███████╗ ██████╗ ██████╗  ██████╗ ███████╗</span>
            <span class="rust-orange bold">██╔════╝██╔═══██╗██╔══██╗██╔════╝ ██╔════╝</span>
            <span class="rust-orange bold">█████╗  ██║   ██║██████╔╝██║  ███╗█████╗  </span>
            <span class="rust-orange bold">██╔══╝  ██║   ██║██╔══██╗██║   ██║██╔══╝  </span>
            <span class="rust-orange bold">██║     ╚██████╔╝██║  ██║╚██████╔╝███████╗</span>
            <span class="rust-orange bold">╚═╝      ╚═════╝ ╚═╝  ╚═╝ ╚═════╝ ╚══════╝</span>

                          <span class="dim">v1.0.53+3cda0dee</span>
                          <span class="dim">claude 2.1.263</span>

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

| Key | Action |
|---|---|
| <kbd>↑</kbd> <kbd>↓</kbd> / <kbd>k</kbd> <kbd>j</kbd> | Navigate (the list scrolls with the selection) |
| <kbd>Enter</kbd> | Open / start / no-op (see click intent) |
| <kbd>r</kbd> | Retry - only when the row is Failed |
| <kbd>?</kbd> | Toggle help (≡ `/help`) |
| <kbd>Esc</kbd> | No-op |
| <kbd>Ctrl+Q</kbd> | Quit (≡ `/quit`) |
| <kbd>Cmd+Left</kbd> <kbd>Cmd+Right</kbd> (<kbd>Ctrl+</kbd> off macOS) | Swallowed (no panes to toggle) |

Enter follows the selected row's lifecycle, the footer hint labeling it: Idle / Running / Attention / AuthRequired / LoggedOut → `enter  open`, switching to the session; Sleeping → `enter  start`, spawning and staying here until the row reaches Idle (avoiding the chat's Connecting stub); Spawning → `enter  ⏳ spawning…`, a no-op; Failed → Enter is a no-op, <kbd>r</kbd> retries.

No input area, so slash commands are keys: `/help` ≡ <kbd>?</kbd>, `/quit` ≡ <kbd>Ctrl+Q</kbd>; `/config` and `/plugins` need a picked project first.

<details>
<summary>Wordmark, colors, spinner, tab title</summary>

- The wordmark is a 43x6 ANSI Shadow figlet in rust orange bold; version lines, org headers, tree connectors, footer hint, account hints and pending glyphs (`○`) are dim; the idle `●` and the running / spawning spinner are rust orange; a failed `✗` is error red; the update indicator (`↑ vX.Y.Z available`) is rust orange; the scrollbar (overflow only) is a rust-orange thumb over a dim track in a 1-col gutter at the box's right edge.
- `[ui] spinner` (one of `braille` / `phase_of_moon` / `ember` / `bars_v` / `star` / `sparkle`, default `braille`) is the active spinner style for every animated surface; `/spinner` overrides it live and persists to the machine-local store (see [the /spinner picker](./pickers.md)). `[ui] fps` (30-240, default 120) is how often the loop repaints while a spinner animates; the gate never goes coarser than 30 ms, out-of-range values clamp with a warning without stopping boot, and a reduced-motion preference keeps its own fixed cadence. No frame rate makes a spinner spin faster than its style asks.
- The terminal tab title tracks activity, not the turn: it pulses whenever anything is happening - a turn, a compaction, live background work in any session, a model download - and goes idle otherwise, deliberately diverging from the chat when a turn ends with backgrounded work still running. Its pulse runs on its own fixed 30 ms step, not on `fps`.

</details>

## Lifecycle rows

| State | Glyph | Row |
|---|---|---|
| Connected (Idle) | `●` rust orange | bold name, relative time - clickable |
| Spawning | spinner frame, rust orange | dim name (not bold) - not yet clickable |
| Sleeping / AuthRequired / LoggedOut / Attention | `○` dim | dim circle, dim name |
| Failed | `✗` red | bold name + a dim-red error sub-row beneath |

<div class="term">

  <pre class="indent">
        ├─ <span class="rust-orange">●</span>  <span class="bold">forge        </span><span class="dim">(personal)        now</span>   <span class="dim"># Connected (Idle): RUST_ORANGE bullet, bold name, relative-time - clickable</span>
        ├─ <span class="rust-orange">⠹</span>  <span class="dim">data-modules   (personal)   spawning</span>   <span class="dim"># Spawning: spinner frame in RUST_ORANGE, dim name (not bold) signals "not yet clickable"</span>
        ├─ <span class="dim">○  web-api      (personal)         2d</span>   <span class="dim"># Sleeping / AuthRequired / LoggedOut / Attention: dim circle, dim name</span>
        └─ <span style="color:#cf6171">✗</span>  <span class="bold">core-v1      </span><span class="dim">(gateway)       failed</span>
              <span style="color:#cf6171">OAuth token expired; run /login to retry...</span></pre>

</div>

The selected row's `▶` arrow renders rust orange bold in a reserved 2-cell column, the rest of the row keeping native colors; when the selected row is Spawning the arrow drops to dim - Enter will not take you anywhere yet. A Failed row's error renders as a dim-red sub-row beneath the name, truncated with `...`; the footer hint shows `r  retry` when the selected row is Failed.

## Account loading gate

The launchpad blocks project-row clicks until every account reaches a terminal state - one probe round-trip each, no retry loop (a usage-endpoint 429 is not inference limiting). Boot holds on [preflight](./preflight.md); mid-session the 60 s poll can flip an account Ready → Bailed on a 401 and back once the credential heals, the gate counting Bailed as terminal throughout. The chip row appears only while some account is non-Ready, hiding rather than showing a permanently green line; the gate is global - an account no project uses can still block the picker.

<div class="term">

  <pre class="indent">
                                  <span class="dim">v1.0.53+3cda0dee</span>
                                  <span class="dim">claude 2.1.263</span>

          <span style="color:#cfc26b">○</span> <span class="dim">gateway</span>   <span style="color:#cfc26b">○</span> <span class="dim">gateway1</span>   <span style="color:#7eb87a">●</span> <span class="dim">personal</span>   <span style="color:#cfc26b">⚠</span> <span class="dim">stargate</span> <span style="color:#cfc26b">- rate limited (retry after 3600s)</span>

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

 <span class="dim">↑↓  navigate     enter  ⏳ loading accounts…     ?  help     ctrl+q  quit</span></pre>

</div>

| Chip glyph | State |
|---|---|
| `○` yellow | Loading - probe in flight |
| `●` green | Ready - available for assignment |
| `⚠` red | Bailed on an auth failure (rejected or expired credentials; repair is an env edit plus a restart) |
| `⚠` yellow | Bailed on a transient failure (rate limit, unreachable endpoint, malformed response; the pollers heal it) |

A bailed chip appends its reason after the name - `- rate limited (retry after 3600s)`, `- unauthorized` for auth classes. While any account is non-terminal every project row downgrades to a blocked click and the footer reads `⏳ loading accounts...`; once the gate lifts rows un-dim and Enter resumes.

<details>
<summary>Per-project pool and account chips</summary>

- A project's pool comes from a six-tier walk over its primary and fallback accounts, first non-empty tier winning: primaries Ready and not at the usage cap, then fallbacks Ready and not capped, then primaries at the cap (a session lands on a saturated 100% account only when every earlier tier is empty), then fallbacks at the cap, then the project's Bailed accounts - primaries before fallbacks (a rate-limited probe hits the usage endpoint, not inference, so spawning on one is legitimate) - then dark. Only when every tier is empty does the project render a dim `no usable accounts` sub-row and stay unclickable after the gate lifts.
- Each project row carries a trailing `(<account>)` chip showing the lead session's assigned account; one worker row nests under the project per persisted dynamic worker, each with the same four elements - lifecycle glyph, name, its own chip, and the right-aligned activity column (<code>&mdash;</code> for a worker that is neither spawning nor failed). A worker's chip and its glyph are independent: a label that has not spawned this boot renders no chip, and a persisted row with no live worker shows `○` regardless. Chip color tracks the assigned account's runtime state: dim = normal; warning yellow, no glyph = at the usage cap (the session still spawns but will throttle) or bailed on a transient failure; error red + `⚠` = bailed on an auth failure. Worker rows are info-only - selection stays on the project row.
- Recovery: the 60 s poll re-probes Bailed accounts once their recorded hold-down passes - the server `Retry-After` for a 429, exponential backoff otherwise, both capped at 10 minutes. A healed account flips Bailed → Ready and the assignment plan recomputes with a frozen overlay: existing sessions keep their boot-time account, only new sessions pick up the recovered one.

</details>
