# Pickers and overlays

## Spinner picker (`/spinner`)

`/spinner` (no arg) opens a transient centered overlay over the active view; `/spinner <name>` sets the style directly. Live picker for the spinner style driving every animated spinner in forge: all six styles list with their live glyph, key and cadence, and moving the highlight previews it across the whole UI.

<div class="term">

  <pre class="indent">
                    <span class="dim">┌─ spinner ───────────────────────────┐</span>
                    <span class="dim">│</span> <span class="accent-bold">▶ ⠹  braille  ·  32ms</span>                <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">◐  phase_of_moon  ·  90ms</span>           <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">✦  ember  ·  160ms</span>                  <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">▆  bars_v  ·  70ms</span>                  <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">✹  star  ·  130ms</span>                   <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">✧  sparkle  ·  160ms</span>                <span class="dim">│</span>
                    <span class="dim">│</span>                                     <span class="dim">│</span>
                    <span class="dim">│</span> <span class="dim">↑↓ preview   enter apply   esc cancel</span> <span class="dim">│</span>
                    <span class="dim">└─────────────────────────────────────┘</span></pre>

</div>

| Key | Action |
|---|---|
| <kbd>↑</kbd> <kbd>↓</kbd> | Move the highlight, live-previewing that style |
| <kbd>Enter</kbd> | Commit the highlighted style (persist) and close |
| <kbd>Esc</kbd> | Restore the pre-overlay style (no persist) and close |

| Style | Cadence | Glyphs |
|---|---|---|
| `braille` | 32 ms | 10-frame spin, full rotation every 320 ms |
| `phase_of_moon` | 90 ms | ◐◓◑◒ |
| `ember` | 160 ms | · ✦ ✧ |
| `bars_v` | 70 ms | ▁▂▃▄▅▆▇█ rising then falling |
| `star` | 130 ms | ✶✸✹✺ |
| `sparkle` | 160 ms | ✦✧✩✪ |

<details>
<summary>Spinner picker details</summary>

- The styles are design intent, not a guarantee: a style never animates quicker than `[ui] fps` can paint - a clamp currently dormant, since the quickest style (braille, 32 ms) outruns the 30 ms repaint floor, so every style runs at its own intent. Reduced motion floors the fast styles. The cadence shown is the style's intent.
- <kbd>Enter</kbd> and `/spinner <name>` write the choice to the machine-local store, never `forge.toml`; at the next boot the stored override layers over the `[ui] spinner` default, which falls back to `braille`. The store write affects only later launches.
- The overlay is modal, over chat and launchpad alike; fully transient - only the committed style persists, and it works identically from both views. Keyboard-only; mouse-click selection is a possible follow-up. Colors: border and selection marker rust orange; highlighted row rust orange bold; unselected key/cadence and hints dim.

</details>

## Model picker (`/model`)

`/model` (no arg) opens the overlay when the session advertises models - otherwise the info line stays - and `/model <id>` switches directly. Rows are the session's available models: the curated OpenRouter catalog on an `openrouter` account, the CLI-advertised models elsewhere; the pseudo `default` row is hidden. Opens with the highlight on the running model beside a `●` marker.

<div class="term">

  <pre class="indent">
                    <span class="dim">┌─ model ───────────────────────────────────────────────────────────┐</span>
                    <span class="dim">│</span> <span class="accent-bold">▶ Z.ai: GLM 5.3 (Opus-class)  SWE-bench V 97% (vals.ai) · $4.40/M</span> <span class="dim">│</span>
                    <span class="dim">│</span>   DeepSeek: DeepSeek V4 Pro 0813 (Opus-class)  <span class="dim">96.4% / 80.6% · $3.20/M</span> <span class="dim">│</span>
                    <span class="dim">│</span>   MoonshotAI: Kimi K3 (Opus-class)  <span class="dim">SWE-bench V 93.4% (anotherwrapper)</span> <span class="dim">│</span>
                    <span class="dim">│</span> <span class="accent">●</span> Z.ai: GLM 5.3 Flash (Opus-class)  <span class="dim">~93% (vals.ai, independent)</span>      <span class="dim">│</span>
                    <span class="dim">│</span>   DeepSeek: DeepSeek V4 Flash (Strong)  <span class="dim">SWE-bench V 91% (vals.ai)</span>     <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">...</span>                                                               <span class="dim">│</span>
                    <span class="dim">│</span>                                                                     <span class="dim">│</span>
                    <span class="dim">│</span> <span class="dim">↑↓ move   enter switch   esc cancel   ● current</span>                     <span class="dim">│</span>
                    <span class="dim">└───────────────────────────────────────────────────────────────────┘</span></pre>

</div>

| Key | Action |
|---|---|
| <kbd>↑</kbd> <kbd>↓</kbd> | Move the highlight (wrapping) |
| <kbd>Enter</kbd> | Switch to the highlighted model and close |
| <kbd>Esc</kbd> | Close without switching |

<details>
<summary>Model picker details</summary>

- <kbd>Enter</kbd> takes the same path as `/model <id>`: an optimistic footer-chip update, then the dispatch - the CLI confirms the live model on its next frame. The rows snapshot carries the session they came from; if the active session changed between open and commit, the commit is refused with a visible notice instead of dispatching. The pseudo-`default` row filter is the same one the `/model` argument autocomplete applies, and long descriptions clip at the overlay edge.
- The overlay is modal, and clicks behind it are ignored - the guard is shared by every picker overlay, so a pane click cannot switch the active session under an open modal. Fully transient: rows snapshot at open, nothing persists. Keyboard-only; mouse-click selection is a possible follow-up. Opened from the chat input only (the launchpad has no input to type `/model` into). The highlight seeds to the running model - matched by requested id, then resolved id, case-insensitively - falling back to row 0 when it is not among the rows.
- Colors: border, selection marker and the current-model dot rust orange; the highlighted name rust orange bold; unselected descriptions and hints dim.

</details>

## Account picker (`/account`)

Idle-only - mid-turn it is a no-op with a red system notice ("Finish or cancel the current turn before switching accounts."). Switch the live session to a different account, keeping the SAME conversation. Rows: the project's allowed accounts, the org's fallbacks (dim `FALLBACK` group), and every `experimental = true` account (dim `EXPERIMENTAL` group - excluded from auto-assignment, offered here globally). Each row: the current `●` marker, the name, a budget block shaped by the billing kind, and a status tag - `usable` green, or red `limit hit` / `auth failed or expired`.

<div class="term">

  <pre class="indent">
       <span class="accent">┌──────────────────────────────────────────────────────────────┐</span>
       <span class="accent">│</span> <span class="accent-bold">Switch account · forge</span>                            <span class="dim">7 accounts</span> <span class="accent">│</span>
       <span class="accent">│</span>                                                              <span class="accent">│</span>
       <span class="accent">│</span> <span class="accent">●</span> <span class="accent-bold">Gateway</span>    5h <span class="error">100%</span>  7d <span class="warning">63%</span>  <span class="warning">⟳ resets 1h 42m</span>      <span class="error">limit hit</span> <span class="accent">│</span>
       <span class="accent">│</span>   Gateway1   5h <span class="success">34%</span>  7d <span class="success">22%</span>                           <span class="success">usable</span> <span class="accent">│</span>
       <span class="accent">│</span>   Personal   5h <span class="dim">-</span>  7d <span class="dim">-</span>               <span class="error">auth failed or expired</span> <span class="accent">│</span>
       <span class="accent">│</span>                                                              <span class="accent">│</span>
       <span class="accent">│</span>   <span class="dim bold">FALLBACK</span>                                                   <span class="accent">│</span>
       <span class="accent">│</span>   Router     <span class="success">$0.56</span> <span class="dim">d</span> <span class="success">$1.25</span> <span class="dim">w</span> <span class="success">$20.30</span> <span class="dim">m</span>      <span class="dim">fallback</span> <span class="dim">·</span> <span class="success">usable</span> <span class="accent">│</span>
       <span class="accent">│</span>                                                              <span class="accent">│</span>
       <span class="accent">│</span>   <span class="dim bold">EXPERIMENTAL</span>                                               <span class="accent">│</span>
       <span class="accent">│</span>   Codex      5h <span class="success">20%</span>  7d <span class="success">8%</span>             <span class="experimental">experimental</span> <span class="dim">·</span> <span class="success">usable</span> <span class="accent">│</span>
       <span class="accent">│</span>   OpenRouter <span class="success">$0.56</span> <span class="dim">d</span> <span class="success">$1.25</span> <span class="dim">w</span> <span class="success">$20.30</span> <span class="dim">m</span>  <span class="experimental">experimental</span> <span class="dim">·</span> <span class="success">usable</span> <span class="accent">│</span>
       <span class="accent">│</span>   Boot       <span class="success">$-</span> <span class="dim">d</span> <span class="success">$-</span> <span class="dim">w</span> <span class="success">$-</span> <span class="dim">m</span>            <span class="experimental">experimental</span> <span class="dim">·</span> <span class="success">usable</span> <span class="accent">│</span>
       <span class="accent">│</span>                                                              <span class="accent">│</span>
       <span class="accent">│</span> <span class="dim">↑↓ move   enter switch   esc cancel   ● current</span>              <span class="accent">│</span>
       <span class="accent">└──────────────────────────────────────────────────────────────┘</span></pre>

</div>

| Key | Action |
|---|---|
| <kbd>↑</kbd> <kbd>↓</kbd> | Move the highlight (clamped, no wrap) |
| <kbd>Enter</kbd> | Switch to the highlighted account (no-op when already current) |
| <kbd>Esc</kbd> | Close |

<details>
<summary>Account picker details</summary>

- A window-billed account (`anthropic`, `codex`, `zai`) renders `5h` + `7d` utilization coloured by proximity to the cap, plus a reset ETA shown only while at the cap. An API-billed one (`openrouter`) has no window, so it renders per-key spend `d` / `w` / `m`; account-wide balance is deliberately absent. Columns with no reading render `-` rather than a zero, following the billing model - an unprobed API account shows `$- d $- w $- m`.
- The budget block follows the billing kind: `Unknown` when no snapshot has landed or the cached one was written under a different `provider` (that last case also logs a warning naming the account - a stale row survives a `forge.toml` edit and is re-seeded at every boot); `Subscription` carrying 5h/7d utilization plus the reset ETA; `Api` carrying the three spend figures. The three `-` states are not a measured zero: no snapshot yet, a snapshot carrying no figure for that column (documented on the proxy path; on the Anthropic path a 200 carrying only the session window), and the stale-provider case. The block degrades - dropping the reset ETA, then the repeated `$`, then the spacing, then to the monthly figure alone - rather than overflowing; the paragraph does not wrap and an overrun is cut with no ellipsis, and the grouped-row tag gives way only after the budget block and the name column.
- Which red reason can apply is keyed on the billing kind: only a window-billed account can saturate (`limit hit`); a probe blocked or bailed account reads `auth failed or expired` either way. Experimental accounts are still probed, so a down one reads an unusable tag like any account.
- Picking a row re-spawns the session under that account's `config_dir` and `claude --resume`s the same session id - the account config dirs share `~/.claude/projects` via symlink, so nothing is copied; the chat re-seeds and the account label refreshes. Picking the current account is a no-op close; an unusable one is shown red. Live workers and in-flight peer asks are not blocked - workers run their own accounts; peer asks to the switched session expire on reconnect and are re-askable.
- Auto-switch on rate-limit is deferred; no account management or `forge.toml` editing from the picker.
- Colors: border and title rust orange; account count, period letters, the empty `5h`/`7d` dashes, the `-` on an unprobed row and the hints dim (the `$-` of an empty spend column keeps the spend colour so the three periods stay one row); window percentages green under ~70, yellow under 100, red at the cap; spend amounts green flat - an uncapped key has no cap to be near; reset ETA yellow; `usable` green; both red reasons red; group headers dim bold; `fallback` dim; `experimental` amber.

</details>

## Dictate overlay (`/dictate`)

In chat only, no turn gating: session-scoped overrides for the normalizer's three prompt axes, plus an input-device pick shared by every session. The `●` marks the value in force - the session override, else the crate default (voice semi-formal, structure prose, destination plain text); a session-set row adds a dim `· this session` suffix. Row labels are permissions, not promises.

<div class="term">

  <pre class="indent">
              <span class="accent">┌────────────────────────────────────────────────────────────┐</span>
              <span class="accent">│</span> <span class="accent-bold">Dictate</span>             <span class="dim">axes this session · device until restart</span> <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="dim">VOICE</span>                                                      <span class="accent">│</span>
              <span class="accent">│</span>     casual                                                   <span class="accent">│</span>
              <span class="accent">│</span>     semi-casual                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span> <span class="accent">▸</span> <span class="bold">semi-formal</span>                                           <span class="accent">│</span>
              <span class="accent">│</span>     formal                                                   <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="dim">STRUCTURE</span>                                                  <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span>   prose                                                  <span class="accent">│</span>
              <span class="accent">│</span>     may bullet a list                                        <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="dim">DESTINATION</span>                                                <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span>   plain text                                             <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span>   email layout  <span class="dim">· this session</span>                          <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="dim">INPUT DEVICE</span>                                               <span class="accent">│</span>
              <span class="accent">│</span>     Device: Focusrite Scarlett 2i2      <span class="accent">active until restart</span> <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>     Reset all to defaults                                    <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">These let the cleanup model do something. They do not</span>        <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">promise it will: text that comes back unchanged means</span>        <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">it declined, not that the setting failed.</span>                    <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">↑↓ move   enter set   esc close   ● in force</span>                 <span class="accent">│</span>
              <span class="accent">└────────────────────────────────────────────────────────────┘</span>

              <span class="accent">┌────────────────────────────────────────────────────────────┐</span>
              <span class="accent">│</span> <span class="accent-bold">Input device</span>                             <span class="dim">reverts on restart</span> <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">▸</span> <span class="bold">System default</span>   <span class="dim">no pin · now: MacBook Pro Microphone</span>   <span class="accent">│</span>
              <span class="accent">│</span>     MacBook Pro Microphone   <span class="dim">system default input</span>           <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span> <span class="bold">Focusrite Scarlett 2i2</span>                                    <span class="accent">│</span>
              <span class="accent">│</span>     Shure SM7B                                                <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">A pin follows the device id: unplugging it fails the take</span>    <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">instead of quietly recording on another input.</span>               <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">A pick lasts until restart; forge.toml keeps the default.</span>    <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">↑↓ move   enter select   esc back   ● in force</span>               <span class="accent">│</span>
              <span class="accent">└────────────────────────────────────────────────────────────┘</span></pre>

</div>

| Key | Action |
|---|---|
| <kbd>↑</kbd> <kbd>↓</kbd> | Move over the selectable rows (the inert reset and stale-pin rows are skipped) |
| <kbd>Enter</kbd> | Set the row and stay open; on INPUT DEVICE, opens pick mode |
| <kbd>Esc</kbd> | In pick mode, steps back to the options body; otherwise closes |

<details>
<summary>Dictate overlay details</summary>

- The axes: `VOICE` (casual / semi-casual / semi-formal / formal), `STRUCTURE` (prose / `may bullet a list`), `DESTINATION` (plain text / email layout) - `INPUT DEVICE` is its own block below the groups, never a fourth axis.
- The device pick is the `/spinner` shape: the `[dictate] device` key is the durable pin and the default shown, a pick overrides it for every session, and a restart reverts to the key. In pick mode `System default` leads as the unpin row, the enumerated inputs follow, and the in-force row carries the `●`; a pin whose device no longer enumerates trails dim, unreachable, with a red `not present · pinned in forge.toml` tag and a two-line red note in place of the standard note, and the footer drops the `● in force` legend since nothing is in force; a machine with no inputs draws `No input devices found.` with an <kbd>Esc</kbd>-only footer.
- One `Reset all to defaults` row sits below the device block, dim and unreachable when nothing is overridden; it clears the device pick along with the override axes - back to defaults means back to forge.toml. There is no per-axis clear. `k` / `ngram` are deliberately absent - they change wall clock and nothing else.
- Scopes: the overrides are session-scoped - every session starts from the crate defaults and they die with the session. The device pick is workspace-scoped: it covers every session, survives a session ending or being replaced, and dies with the process. The pin is durable and lives only in `forge.toml`, which the dialog never writes. When a capture starts the workspace resolves the pick over the pin (the pick wins; `System` means the system default even over a pin; no pick falls through to the pin); when it finishes, the session's overrides merge over the crate defaults. The device catalog is enumerated off the render thread, cached, and refreshed when the overlay re-opens. <kbd>Enter</kbd> on a set restarts the highlight at the first row.
- Colors: border and title rust orange; the right-justified notes, group headers, the `· this session` suffix, the config/default tags, the inert reset row, the stale-pin row's label and the note lines dim; the highlight cursor rust orange with a white bold label; the in-force `●` rust orange (same glyph and purpose as the account picker's); `active until restart` rust orange; `not present` and the stale-pin note in the error colour; hints dim.
- Push-to-talk itself is the Right Cmd dictate key (below); the recording / transcribing indicator states are the composer surfaces' own.

</details>

## Push-to-talk dictate key (`Right Cmd`)

Hold to record, release to transcribe; a clean tap under 300 ms instead engages a toggle. The key is configurable via `[dictate] bind` (`right_cmd` default, `left_cmd`, `off` - the cmd equivalent on Linux and Windows is right/left Control); the gesture via `[dictate] mode`: `auto` (the timing inference), `toggle` (press starts, the next press stops, releases never stop), or `hold` (release always transcribes, however brief - no tap window). Detection depends on the kitty enhancement flags, negotiated at startup and re-set on resize - a terminal or multiplexer that discards them is logged at boot AND surfaced as a warning row on preflight's Dictation section, because with the flags gone the key silently never arrives.

<details>
<summary>Chords and refusals</summary>

While the key is down, any other key marks the hold a chord (still dispatching normally - Right Cmd + V still pastes) and a chorded release cancels the speculative recording instead of transcribing it. With `[dictate]` disabled the key is dead; a press while dictation is enabled but not usable (the engine still loading, a device that will not open) is refused with a notice naming the cause. Bare modifier presses and releases are consumed app-wide so they cannot tear down autocomplete or disturb a queued paste burst, and held keys arrive as `Repeat` events dispatched like presses.

</details>

| Key | Action |
|---|---|
| <kbd>Right Cmd</kbd> press | Start recording |
| Release after a hold | Finish and transcribe |
| Release after a clean tap | Keep recording |
| Second press + release | Finish |
| Any other key while held | Mark a chord, dispatch normally |
| Chorded release | Cancel the speculative recording (or leave a toggle running) |

<details>
<summary>Where takes land</summary>

A take can start from every text surface - the chat composer, the diff overlay, the prompt dock, and the plugins view - and each carries the pulsing circle blip on a fixed chrome spot (the key-hints bar, the dock's footer hint row, the sub-tab strip) while a take is live. The first <kbd>Esc</kbd> on any of them abandons the take before that surface's own Esc semantics fire. An explicit save or submit of the host editor (<kbd>Enter</kbd> on the comment editor, <kbd>Ctrl+Enter</kbd> on the finish-review modal) abandons the take too - the user submitted without the words - while a passive landing stamps a dim "dictated words landed here" notice on the draft it reached. The transcript lands in whichever editor of the dictating session is focused - the chat draft, a diff comment editor, the finish-review overview, or a plugins field - falling back to that session's chat draft when focus has moved to another session or no editor is open; a truncated take warns where its words landed, and a notice without text covers no-audio (quiet room vs. structural silence worded differently), an empty transcript, a failure, and a mic claimed by another holder in the process.

</details>
