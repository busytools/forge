# Pickers and overlays

## Spinner picker (`/spinner`)

`/spinner` (no arg) opens a transient centered overlay over the active view; `/spinner <name>` sets the style directly. Live picker for the spinner style driving every animated spinner in forge: all six styles list with their live glyph, key and cadence, and moving the highlight previews it across the whole UI.

<div class="term">

  <pre class="indent">
                    <span class="dim">┌─ spinner ─────────────────────────────┐</span>
                    <span class="dim">│</span> <span class="accent-bold">▶ ⠹  braille  ·  32ms</span>                 <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">◐  phase_of_moon  ·  90ms</span>           <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">✦  ember  ·  160ms</span>                  <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">▆  bars_v  ·  70ms</span>                  <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">✹  star  ·  130ms</span>                   <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">✧  sparkle  ·  160ms</span>                <span class="dim">│</span>
                    <span class="dim">│</span>                                       <span class="dim">│</span>
                    <span class="dim">│</span> <span class="dim">↑↓ preview   enter apply   esc cancel</span> <span class="dim">│</span>
                    <span class="dim">└───────────────────────────────────────┘</span></pre>

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

`/model` (no arg) opens the overlay when the session advertises models - otherwise the info line stays - and `/model <id>` switches directly. Rows are the session's org's declared models, authored in forge.toml; the pseudo `default` row is hidden. Opens with the highlight on the running model beside a `●` marker.

<div class="term">

  <pre class="indent">
                    <span class="dim">┌─ model ────────────────────────────────────────────────────────────────┐</span>
                    <span class="dim">│</span> <span class="accent-bold">▶ glm-5.3</span>                                                              <span class="dim">│</span>
                    <span class="dim">│</span>   deepseek-v4-pro-0813                                                 <span class="dim">│</span>
                    <span class="dim">│</span>   kimi-k3                                                              <span class="dim">│</span>
                    <span class="dim">│</span> <span class="accent">●</span> glm-5.3-flash                                                        <span class="dim">│</span>
                    <span class="dim">│</span>   deepseek-v4.1-flash                                                  <span class="dim">│</span>
                    <span class="dim">│</span>   <span class="dim">...</span>                                                                  <span class="dim">│</span>
                    <span class="dim">│</span>                                                                        <span class="dim">│</span>
                    <span class="dim">│</span> <span class="dim">↑↓ move   enter switch   esc cancel   ● current</span>                        <span class="dim">│</span>
                    <span class="dim">└────────────────────────────────────────────────────────────────────────┘</span></pre>

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

## Gateway view (`/gateway`)

Read-only, open any time including mid-turn. It shows what the gateway holds: every org in name order, each org's primary and fallback pins in walk order, and one line per account naming its provider, what it has left, and whether it is pickable. Nothing here picks, rebinds, respawns or rotates - the spawn pick is the only way a session's account is decided, so there is no account to switch to, only a gateway to inspect.

<div class="term">

  <pre class="indent">
       <span class="accent">┌────────────────────────────────────────────────────────────────┐</span>
       <span class="accent">│</span> <span class="accent-bold">Gateway · 2 orgs</span>                                               <span class="accent">│</span>
       <span class="accent">│</span>                                                                <span class="accent">│</span>
       <span class="accent">│</span> <span class="bold">  Busytools</span>                                                    <span class="accent">│</span>
       <span class="accent">│</span> <span class="dim">  primary   Zai, Personal, OpenRouter-TM</span>                       <span class="accent">│</span>
       <span class="accent">│</span> <span class="dim">  fallback  OpenRouter</span>                                         <span class="accent">│</span>
       <span class="accent">│</span>     Zai  zai  5h 100%  7d 63%  resets 1h 42m  <span class="error">limit hit</span>        <span class="accent">│</span>
       <span class="accent">│</span>     Personal  anthropic  5h 34%  7d 22%  <span class="success">usable</span>                <span class="accent">│</span>
       <span class="accent">│</span>     OpenRouter-TM  openrouter  day $0.56  week $1.25  month $20.30  <span class="success">usable</span> <span class="accent">│</span>
       <span class="accent">│</span>     OpenRouter <span class="dim">fallback</span>  openrouter  -  <span class="error">auth failed or expired</span> <span class="accent">│</span>
       <span class="accent">│</span>                                                                <span class="accent">│</span>
       <span class="accent">│</span> <span class="bold">  Subspace</span>                                                      <span class="accent">│</span>
       <span class="accent">│</span> <span class="dim">  primary   Subspace</span>                                            <span class="accent">│</span>
       <span class="accent">│</span> <span class="dim">  fallback  -</span>                                                   <span class="accent">│</span>
       <span class="accent">│</span>     Subspace  anthropic  5h 12%  7d 9%  <span class="success">usable</span>                  <span class="accent">│</span>
       <span class="accent">│</span>                                                                <span class="accent">│</span>
       <span class="accent">│</span> <span class="dim">esc close   read-only: the spawn pick decides every session's account</span> <span class="accent">│</span>
       <span class="accent">└────────────────────────────────────────────────────────────────┘</span></pre>

</div>

| Key | Action |
|---|---|
| <kbd>Esc</kbd> | Close |

<details>
<summary>Gateway view details</summary>

- The snapshot comes from one `Workspace` query, not a `Command`: query refreshes are direct methods under the MVVM contract, and this is a read of state the gateway already holds. Every other key is inert while the overlay is open - it consumes them so the chat beneath never sees them, and it acts on none of them but the close.
- An org with no fallbacks renders `-` rather than a blank: the empty list is the normal shape, not a missing value. A fallback-only account carries a dim `fallback` suffix on its line, because the pins above it already say which list it came from.
- The budget follows the billing kind, compactly: a window-billed account (`anthropic`, `codex`, `zai`) renders `5h` + `7d` utilization, with `resets <when>` while it is at its cap; an API-billed one (`openrouter`) renders its per-key `day` / `week` / `month` spend. A column with no reading renders `-` rather than a zero, and an account with no snapshot at all renders a single `-`.
- The state tag is the pool's own verdict: `usable` green, `limit hit` red for a capped window, `auth failed or expired` red for a blocked probe or a bail.
- Colors: border, title and the org count rust orange; org names bold; the pins, the hints and the `fallback` suffix dim; account names bold; the budget figures plain, since the state tag at the end of the line is what carries the verdict; `usable` green; both red reasons red.

</details>

## Dictate overlay (`/dictate`)

In chat only, no turn gating: session-scoped overrides for the normalizer's three prompt axes, plus an input-device pick shared by every session. The `●` marks the value in force - the session override, else the crate default (voice semi-formal, structure prose, destination plain text); a session-set row adds a dim `· this session` suffix. Row labels are permissions, not promises.

<div class="term">

  <pre class="indent">
              <span class="accent">┌──────────────────────────────────────────────────────────────┐</span>
              <span class="accent">│</span> <span class="accent-bold">Dictate</span>             <span class="dim">axes this session · device until restart</span> <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="dim">VOICE</span>                                                      <span class="accent">│</span>
              <span class="accent">│</span>     casual                                                   <span class="accent">│</span>
              <span class="accent">│</span>     semi-casual                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span> <span class="accent">▸</span> <span class="bold">semi-formal</span>                                            <span class="accent">│</span>
              <span class="accent">│</span>     formal                                                   <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="dim">STRUCTURE</span>                                                  <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span>   prose                                                  <span class="accent">│</span>
              <span class="accent">│</span>     may bullet a list                                        <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="dim">DESTINATION</span>                                                <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span>   plain text                                             <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span>   email layout  <span class="dim">· this session</span>                           <span class="accent">│</span>
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
              <span class="accent">└──────────────────────────────────────────────────────────────┘</span>

              <span class="accent">┌──────────────────────────────────────────────────────────────┐</span>
              <span class="accent">│</span> <span class="accent-bold">Input device</span>                             <span class="dim">reverts on restart</span>  <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">▸</span> <span class="bold">System default</span>   <span class="dim">no pin · now: MacBook Pro Microphone</span>    <span class="accent">│</span>
              <span class="accent">│</span>     MacBook Pro Microphone   <span class="dim">system default input</span>            <span class="accent">│</span>
              <span class="accent">│</span>   <span class="accent">●</span> <span class="bold">Focusrite Scarlett 2i2</span>                                   <span class="accent">│</span>
              <span class="accent">│</span>     Shure SM7B                                               <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">A pin follows the device id: unplugging it fails the take</span>    <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">instead of quietly recording on another input.</span>               <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">A pick lasts until restart; forge.toml keeps the default.</span>    <span class="accent">│</span>
              <span class="accent">│</span>                                                              <span class="accent">│</span>
              <span class="accent">│</span> <span class="dim">↑↓ move   enter select   esc back   ● in force</span>               <span class="accent">│</span>
              <span class="accent">└──────────────────────────────────────────────────────────────┘</span></pre>

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
