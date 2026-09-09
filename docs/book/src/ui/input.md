# Input area + autocomplete

## Input area (default state)

*visible: in `ActiveView::Chat` (hidden when a full-frame view is active). Height grows from 1 row up to `MAX_INPUT_HEIGHT = 50` as the user types.*

Backed by `tui-textarea` for cursor / selection / paste-burst handling. The box is a `BorderType::Thick` `Borders::ALL` block drawn by `input.rs` in RUST_ORANGE bold - no separate separator rows. Height grows from 1 row up to `MAX_INPUT_HEIGHT = 50` as the user types or pastes multi-line content. Prompt char is `➤` (`theme::PROMPT_CHAR`, U+27A4) in RUST_ORANGE. Placeholder text is in DIM italic. Slash commands typed in the input are coloured `SLASH_COMMAND` (light magenta) by `input.rs`.

Above the input there's a **hint slot** that grows the input region's height to accommodate. Possible hint rows (each takes 1+ lines):

- **Login hint** (when CLI reports `app.login_hint`): "*Authentication required: \<method\> -- \<description\>*" in Yellow + dim hint "*Run \`claude auth login\` in another terminal to authenticate*" beneath. Two lines.
- **Cancel hint** (during cancellation): spinner + "*Cancelling current turn... draft will auto-submit when ready.*" in DIM. One line.
- **Prompt suggestion**: "*Suggestion: \<text\>    Tab to accept*" - "Suggestion:" in DIM, suggestion in white, accept-hint in DIM. One line.

When `app.status == AppStatus::Connecting`, the entire input area is replaced with a spinner + "*Connecting to Claude Code...*" in DIM.

<div class="term">

  <pre class="indent">
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                        <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span></pre>

</div>

*With a login-required hint above the input:*

<div class="term">

  <pre class="indent">
  <span class="warning">Authentication required: claude.ai -- Anthropic OAuth (Pro)</span>
  <span class="dim">Run `claude auth login` in another terminal to authenticate</span>
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                        <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span></pre>

</div>

### Dictation status row

With `[dictate] enabled` and the models loaded (one `SessionUpdate::DictateAvailability` after preflight), a take lives entirely inside the composer's interior. Idle reserves nothing: the box is exactly as tall as the draft, and the old design's top-border meter cells are gone. When a recording starts the interior grows one row - the status row, the same slot the notice row uses, so the two never coexist (a stamped notice keeps the slot and the status row stands down). When the take resolves the row collapses and the box shrinks back. Level readings arrive as `DictateLevel` events every 50 ms, each the peak over the window since the previous one off the take-and-reset `CaptureMeter::level`. A long take is cut into segments at measured pause boundaries, and each segment transcribes while the microphone is still recording - the row shows those words as a settled count long before the speaker stops.

<div class="term">

  <pre class="indent">
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>   recording - the interior grows one row; settled segments count on the label
  <span class="accent">┃</span>  <span class="accent">●</span> <span class="accent">0:07</span> <span style="color:rgb(255,176,88)">-18 dB</span> <span class="dim">listening · 2 ready</span> <span class="dim">▁▂</span><span style="color:rgb(171,97,0)">▄▆</span><span style="color:rgb(255,176,88)">█</span><span style="color:rgb(171,97,0)">▅▃▂</span><span class="dim">▁▁</span><span style="color:rgb(171,97,0)">▃▅▆</span><span style="color:rgb(255,176,88)">█</span><span style="color:rgb(171,97,0)">▆▄▂</span><span class="dim">▁▁</span><span style="color:rgb(171,97,0)">▂▄</span><span style="color:rgb(255,176,88)">▆</span><span style="color:rgb(171,97,0)">▄▃▂</span><span class="dim">▁</span>       <span class="dim">esc cancel</span><span class="accent">┃</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                            <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span></pre>

</div>

The row's anatomy is identical across both live states - indicator dot, `m:ss` timer, the live dB figure, label, meter, right-aligned esc hint - and only colour and freeze change on the handoff. While recording: an orange dot `●` pulsing on a 1.05 s cycle (held steady under reduced motion), an orange timer live off the take's own start stamp, a DIM `listening` label that grows a settled-segment count (`listening · 2 ready`) as `DictateProgress` steps arrive from the pipelined segments, and a 26-cell meter. The meter is normalized display-side, in `forge-tui`: the raw peak feed runs through an envelope in the dB domain (attack 0.6, release 0.25 per 50 ms tick - fast up, slower down), then scales against the envelope's own recent dynamic range - a max follower that grabs a peak and forgets it slowly, a min follower that catches valleys and climbs slowly back off them - so the ramp shows speech shape whatever the mic's AGC does to the overall level. The span never drops under 8 dB and carries 2 dB of headroom, so a fresh peak maps just under full scale; the feed is gated at the take's own silence floor, then softened with a 0.9 gamma onto the block ramp `▁▂▃▄▅▆▇█`. Cells at or under the gate draw the floor glyph in DIM - so a bar that never leaves the floor is the same structural silence `Outcome::NoAudio` reports - and everything above grades dim through orange toward the hot tint rgb(255,176,88) by value. The composer border rides along: its colour eases (0.12 per 50 ms tick, time-scaled, no extra clock) toward the hot tint in proportion to the current level, never more than 35% of the way.

Between the timer and the label rides the live dB figure, such as `-18 dB`: its text is the held reading refreshed at 5 Hz whatever the frame rate, and its colour follows the current level - DIM while the feed sits at or under the gate, grading through orange toward the hot tint above it. The figure lives entirely in the status row, so the draft is never painted over and the caret stays the normal blinking block in every state, recording included. While transcribing the figure holds its last reading, DIM alongside the frozen meter.

<div class="term">

  <pre class="indent">
  <span style="color:rgb(97,160,224)">┏</span><span style="color:rgb(97,160,224)">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span style="color:rgb(97,160,224)">┓</span>   transcribing - same anatomy, frozen and dimmed toward blue; multi-window takes tally the window on the label
  <span style="color:rgb(97,160,224)">┃</span>  <span style="color:rgb(97,160,224)">◌</span> <span class="dim">0:07</span> <span class="dim">-18 dB</span> <span class="dim">transcribing 2/6</span> <span style="color:rgb(38,53,74)">▁▂</span><span style="color:rgb(47,72,109)">▄▆</span><span style="color:rgb(54,85,130)">█</span><span style="color:rgb(47,72,109)">▅▃▂</span><span style="color:rgb(38,53,74)">▁▁</span><span style="color:rgb(47,72,109)">▃▅▆</span><span style="color:rgb(54,85,130)">█</span><span style="color:rgb(47,72,109)">▆▄▂</span><span style="color:rgb(38,53,74)">▁▁</span><span style="color:rgb(47,72,109)">▂▄</span><span style="color:rgb(54,85,130)">▆</span><span style="color:rgb(47,72,109)">▄▃▂</span><span style="color:rgb(38,53,74)">▁</span>   <span class="dim">esc cancel</span><span style="color:rgb(97,160,224)">┃</span>
  <span style="color:rgb(97,160,224)">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                        <span style="color:rgb(97,160,224)">┃</span>
  <span style="color:rgb(97,160,224)">┗</span><span style="color:rgb(97,160,224)">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span style="color:rgb(97,160,224)">┛</span></pre>

</div>

The row renders the moment the phase flips, however brief the transcription - warm takes (roughly 115 ms for a 5-second clip) are visible too. The transcribing row keeps the same anatomy: a blue pulsing dot `◌` rgb(97,160,224), the timer frozen at the take's length and DIM, a DIM `transcribing` label, and the meter frozen at its last recording frame - no new animation appears out of nowhere; the cells only change colour, tinting dim toward blue. The border eases toward the same blue the moment the handoff lands. Pipelined segments have been settling since recording began; the stop makes the total known and the label tallies the remainder as `DictateProgress` steps arrive - `transcribing 2/6` counts segments settled against the final total, so only the tail is usually left. The row collapses the moment the take resolves; single-segment takes, the overwhelming case, never show a tally.

**Done.** No landed row and no landed label. The row collapses, the text pastes at the caret, and the border takes one green beat rgb(130,199,107) for roughly 450 ms before easing back to the composer's normal orange; once the ease settles the border state is dropped entirely, so idle rendering is untouched by any of this.

<div class="term">

  <pre class="indent">
  <span style="color:rgb(130,199,107)">┏</span><span style="color:rgb(130,199,107)">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span style="color:rgb(130,199,107)">┓</span>   done - one green beat while the text pastes
  <span style="color:rgb(130,199,107)">┃</span> <span class="accent">➤</span> fix the flaky retry test and<span style="color:rgb(130,199,107)">▊</span>                                             <span style="color:rgb(130,199,107)">┃</span>
  <span style="color:rgb(130,199,107)">┗</span><span style="color:rgb(130,199,107)">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span style="color:rgb(130,199,107)">┛</span>

  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>   idle - dictation available, nothing reserved
  <span class="accent">┃</span> <span class="accent">➤</span> fix the flaky retry test and<span class="accent">▊</span>                                             <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span></pre>

</div>

**The notice row.** Everything that is not "text landed" is one row inside the box, above the draft, cleared by the next keystroke - the same slot the status row borrows while a take is live, so a notice always wins it. It is the one row that ever changes the box height, and it only appears when there is something to say:

<div class="term">

  <pre class="indent">
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>
  <span class="accent">┃</span>  <span class="dim">nothing above -50 dBFS in 4s · loudest was -38.2 · try again</span>             <span class="accent">┃</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                        <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span></pre>

</div>

Notices: a quiet room carries its own measured peak and offers a retry (DIM); every sample exactly zero is structural and sticky, so no retry is offered (red); a take that normalised to nothing says so (DIM); a truncated take lands its words plus a keep-going note (yellow); recognition failures are one grouped notice (DIM); a busy microphone names the holder, and a device that would not open is refused before recording starts (both red). Landed text inserts at the caret through the editor's own insertion path - never the paste dispatcher, so long takes are never collapsed to a placeholder - and a copy lands on the system clipboard alongside, so the words survive whatever happens to the draft. The take is bound to the session that started it: results route by session key, so switching tabs mid-transcription never moves the words to another composer. <kbd>Esc</kbd> discards a recording; while transcribing it abandons the ticket and only falls through to turn-cancellation when nothing is in flight.

- **code** - `crates/forge-tui/src/ui/input.rs::render` · meter, status row and border state in `crates/forge-tui/src/app/dictate.rs` · recording lifecycle in `crates/forge-workspace/src/dictate.rs`
- **exits** - <kbd>Esc</kbd> discards a recording and abandons a transcription in flight; it cancels a turn only when no take is live. A recording starts from the [push-to-talk key](./pickers.md).

## Autocomplete dropdown

*visible: while user is typing one of four triggers - `/` (slash), `@` (file mention), `&` (subagent), `:` (emoji)*

Rounded-border dropdown anchored to the input. Title format depends on context: ` Commands (N) ` for slash command names, ` /<cmd> Args (N) ` for slash-command arguments (e.g. ` /model Args (3) `), ` Files & Folders ` for `@`, ` Subagents (N) ` for `&`, ` Emoji ` for `:`. Title rendered in DIM. Border DIM. The visible-row cap is per trigger - `slash::MAX_VISIBLE = 20`, `mention::MAX_VISIBLE = 32` (shared by `@`), `emoji::MAX_VISIBLE = 10`, `subagent::MAX_VISIBLE = 8` - and each is further clamped to the rows that actually fit above or below the input; scrolls in place.

Each item: 3-char selection prefix (` ▸ ` in RUST_ORANGE bold for selected, `   ` for others) + primary text (default fg, with case-insensitive match-substring highlight when there's a query) + optional secondary description (DIM). Slash commands typed in the *input area* are colored `SLASH_COMMAND` (light magenta) by `input.rs` - but **inside the dropdown**, slash items use default fg, not magenta.

<div class="term">

  <pre class="indent">
  <span class="dim">╭ </span><span class="dim">Commands (4)</span><span class="dim"> ───────────────────────────────────────────────────────╮</span>
  <span class="dim">│</span> <span class="accent bold">▸ </span>/clear           <span class="dim">  Clear chat history</span>                            <span class="dim">│</span>
  <span class="dim">│</span>    /compact         <span class="dim">  Compact conversation context</span>                  <span class="dim">│</span>
  <span class="dim">│</span>    /<span style="text-decoration: underline">m</span>odel           <span class="dim">  Switch model</span>                                  <span class="dim">│</span>
  <span class="dim">│</span>    /help            <span class="dim">  Show help</span>                                     <span class="dim">│</span>
  <span class="dim">╰─────────────────────────────────────────────────────────────────────╯</span>
  <span class="accent">❯</span> <span class="slash">/m</span>_</pre>

</div>

*Underlines mark match-highlighted regions (rendered with a different style - fg colour or background, depending on the implementation; not strictly underlines in the running TUI).*

- **code** - `crates/forge-tui/src/ui/autocomplete.rs` · positioning via `choose_dropdown_x` / `choose_dropdown_y` (renders above OR below the input depending on space)
- **four triggers** - `/` = slash · `@` = file mention (paths from `FileIndex`) · `&` = subagent invocation · `:` = emoji shortcode
- **command source** - The slash list (also feeding `/help`) is seeded once at connect from `system/init` `slash_commands`, then **live-refreshed** on every `commands_changed` event (emitted after a plugin/command reload) - the fresh list replaces the seed wholesale, so a newly added command shows immediately without a restart. Both paths parse through `available_commands_from_json` into `UiSession.available_commands`.
- **known issues** - [#46](https://github.com/busytools/forge/issues/46) - pre-init gap (CLI emits init only post-first-turn; fallback list needed)  ·  [#47](https://github.com/busytools/forge/issues/47) - slash commands render as full expanded prompt text on session resume

### `:shortcode:` emoji picker

Slack-shaped: type `:` plus at least two characters and the picker opens; keep typing to filter, <kbd>Enter</kbd> or <kbd>Tab</kbd> replaces the whole token with the glyph and typing continues. Typing the closing `:` of an exact shortcode lands the glyph too, so `:tada:` straight through works. <kbd>Esc</kbd> dismisses the picker only, leaving the typed text alone. Rows are the glyph then its `:name:` with the query match highlighted, in the same rounded-border box as the other three triggers.

Unlike `/`, `@` and `&`, this picker is **not chat-only**: it hangs off `App::focused_input`, so it serves the chat draft, the /diff inline comment editor and the Finish-review overview from one implementation. In the /diff overlay it is the innermost surface - while open it owns <kbd>Esc</kbd>, <kbd>Enter</kbd> and the arrows, so `:` then <kbd>Esc</kbd> cannot fall through to the overlay's "finish review".

<div class="term">

  <pre class="indent">
  <span class="dim">╭ </span><span class="dim">Emoji</span><span class="dim"> ───────────────────────────────────╮</span>
  <span class="dim">│</span> <span class="accent bold">▸ </span>😄   :<span style="text-decoration: underline">sm</span>ile:                          <span class="dim">│</span>
  <span class="dim">│</span>    😃   :<span style="text-decoration: underline">sm</span>iley:                         <span class="dim">│</span>
  <span class="dim">│</span>    😏   :<span style="text-decoration: underline">sm</span>irk:                          <span class="dim">│</span>
  <span class="dim">│</span>    🙂   :<span style="text-decoration: underline">s</span>lightly_s<span style="text-decoration: underline">m</span>iling_face:          <span class="dim">│</span>
  <span class="dim">│</span>    😅   :sweat_<span style="text-decoration: underline">sm</span>ile:                    <span class="dim">│</span>
  <span class="dim">╰──────────────────────────────────────────╯</span>
  <span class="accent">❯</span> nice work :sm_</pre>

</div>

- **code** - `crates/forge-tui/src/app/emoji.rs` (table, trigger detection, ranking, token replacement) · rows shared by both surfaces via `ui/autocomplete.rs::emoji_dropdown_lines` · /diff popup placement in `ui/diff_overlay.rs::render_emoji_dropdown`
- **trigger rule** - The `:` counts only at the start of a line or directly after whitespace, and the query must be `[a-z0-9_+-]` - so `http://`, `10:30`, `note:todo` and `Foo::bar` never open a picker. Same rule `@` uses.
- **emoji data** - A curated static table of ~200 GitHub / Slack shortcodes, sorted by name (a test enforces the ordering and rejects duplicates). Deliberately not the full Unicode set: the long tail is never scrolled to in a typeahead, and a crate carrying every sequence plus metadata is several hundred KB of generated tables for a cosmetic feature.
- **ranking** - Exact match, then prefix matches, then substring matches; ties alphabetical.

# Unified prompt (permission · plan · question)

## Dock-morph widget

*visible: whenever the active session's `prompt_queue` has a head - permission requests, plan-approval (ExitPlanMode), or AskUserQuestion all route through the same widget*

One renderer, three modes (single-select / multi-select / selection+text). When a prompt arrives the chat-input box at the bottom of the screen **morphs** - text-entry slot disappears, the orange thick chrome stays put, options appear inside with a `▸` pointer on the focused row, arrow keys move focus, Enter confirms, Esc cancels.

The tool block above keeps its normal in-progress render (no inline expansion). Per-session FIFO queue: when more than one prompt is pending, a dim "`▼ N more pending after this`" line appears at the top of the dock body; otherwise that line is hidden.

**Common-case permission** - no decision_reason or display_description (most Bash / Edit / Read calls land here):

<div class="term">

  <pre class="indent"><span class="accent bold">┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="bold">Bash · git push origin polish/rate-limit-chip-softer-34</span>             <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="accent bold">▸ </span><span class="success">✓</span> <span class="bold">Allow once</span>                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="success">✓</span> <span class="dim">Allow always for Bash · git push *</span>                              <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="error">✗</span> <span class="dim">Deny</span>                                                            <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="dim">...</span> <span class="dim">Tell Claude something else</span>                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="dim">↑↓ select  ⏎ confirm  esc reject</span>                                    <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛</span></pre>

</div>

**Permission with full CLI context** - Read outside workspace (yellow `⚠` = `decision_reason`, dim line below = `display.description`; "Allow always" entries derive from `permission_suggestions`):

<div class="term">

  <pre class="indent"><span class="accent bold">┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="bold">Read · /tmp/forge-deny-scenario.txt</span>                                 <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span style="color:#d4b73e">⚠ Path is outside allowed working directories</span>                       <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="dim">Reads the contents of a file outside this project.</span>                  <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="accent bold">▸ </span><span class="success">✓</span> <span class="bold">Allow once</span>                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="success">✓</span> <span class="dim">Allow always for Read · paths matching //tmp/**</span>                 <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="error">✗</span> <span class="dim">Deny</span>                                                            <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="dim">...</span> <span class="dim">Tell Claude something else</span>                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="dim">↑↓ select  ⏎ confirm  esc reject</span>                                    <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛</span></pre>

</div>

**AskUserQuestion** - multi-select with notes-option toggled. Question header gets a `?` in RUST_ORANGE, the question body is white below it, options use checkbox markers (`[x]` / `[ ]`) before the icon; toggling the `... Tell Claude something else` entry expands an inline notes editor. The Notes row's `[x]` is display-only - it tracks the live notes buffer (so the user sees confirmation the typed content will be included on submit) AND the wire `annotation.notes` field carries the typed text independently of `selected_option_indices`:

<div class="term">

  <pre class="indent"><span class="accent bold">┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="accent">? </span><span class="bold">Environments (Q2 of 3)</span>                                            <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="bold">Pick the environments to deploy to.</span>                                <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="success">[x]</span> <span class="success">✓</span> <span class="dim">Staging</span>                                                     <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="success">[x]</span> <span class="success">✓</span> <span class="dim">Production</span>                                                  <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="dim">[ ]</span> <span class="success">✓</span> <span class="dim">Development</span>                                                 <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="accent bold">▸ </span><span class="success">[x]</span> <span class="dim">...</span> <span class="bold">Tell Claude something else:</span>                              <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="bold">Also bump the queue worker concurrency_</span>                          <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="dim">space toggle  ↑↓ move  ⏎ submit  esc cancel</span>                         <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛</span></pre>

</div>

**The dictate blip on the dock.** While a take is live, the pulsing circle blip - ● orange while recording, blue while transcribing, gone when the text lands - leads the dock's footer hint row, a fixed chrome spot visible whichever option holds the focus, so dictation is startable and stoppable with the dock up. The blip is the whole indicator on this surface: no timer, no dB figure, no meter. The first <kbd>Esc</kbd> with a take live abandons the take and the dock stands; the next one rejects it. Dictated words land in the notes draft (the chat composer's buffer) either way.

**Queue indicator** - when the queue depth is > 1 (e.g. a background session's prompt is queued behind the active prompt), a dim line at the top of the dock body shows the count:

<div class="term">

  <pre class="indent"><span class="accent bold">┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="dim">▼ 2 more pending after this</span>                                         <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="bold">Edit · src/foo.rs</span>                                                   <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="accent bold">▸ </span><span class="success">✓</span> <span class="bold">Allow once</span>                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="success">✓</span> <span class="dim">Allow always for Edit · src/**</span>                                  <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="error">✗</span> <span class="dim">Deny</span>                                                            <span class="accent bold">┃</span>
<span class="accent bold">┃</span>    <span class="dim">...</span> <span class="dim">Tell Claude something else</span>                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┃</span>  <span class="dim">↑↓ select  ⏎ confirm  esc reject</span>                                    <span class="accent bold">┃</span>
<span class="accent bold">┃</span>                                                                      <span class="accent bold">┃</span>
<span class="accent bold">┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛</span></pre>

</div>

### Icon vocabulary

- ✓ green - `PermissionOptionKind::Allow` (any Allow variant)
- ✗ red - `PermissionOptionKind::Deny` (any Deny variant)
- ✎ blue - `PermissionOptionKind::Edit` (Allow with edits - synthesized for editable tools: Bash, Edit, Write, MultiEdit, NotebookEdit)
- ... dim - `PermissionOptionKind::Notes` (the synthesized "Tell Claude something else" escape hatch; always last in the option list)

### Where options come from

Options are derived from the CLI's `permission_suggestions` field (one of the wire fields forge had been ignoring pre-redesign). Three \`PermissionUpdate\` variants drive the contextual options:

- `addRules` (Read outside workspace) → "Allow always for {tool} · paths matching {pattern}". macOS `/tmp` + `/private/tmp` mirror entries are deduped for display but both rule entries are kept on the wire so the CLI installs both.
- `addDirectories` (Write / Edit outside workspace) → "Allow always & add {dirs} to allowed dirs".
- `setMode` (typically arrives on the prompt that intercepts plan-mode - Write outside workspace) → "Allow always & switch to {mode} mode". Note: ExitPlanMode itself sends `permission_suggestions: null`; the mode switch lives on the earlier intercept prompt, not on ExitPlanMode.

Plus two universally-synthesized options: `... Tell Claude something else` (always last) routes to `deny(notes_text)` on the wire; `✎ Allow with edits` (only for editable tools) routes to `allow_with_input(edited_value)`.

### Keymap

- **option-picker mode (default)** - `↑` / `↓` / `←` / `→` - move option focus (wraps top↔bottom)<br>
  `Home` / `End` - jump to first / last option<br>
  `Space` - multi-select only: toggle current option<br>
  `Enter` - Notes-kind option → Submit (notes text routed from canonical `App.input` editor); Edit-kind option → enter editing-input mode (Consumed); otherwise → Submit<br>
  `Esc` - Cancel<br>
  `Tab` / `BackTab` / printable chars / `Backspace` / `Delete` - when focused option is Notes-kind, route to `App.input` editor (canonical chat-input handler - handles unicode, paste bursts, speech-to-text insertions, cursor navigation). Otherwise Consumed (swallowed).

### State + dispatch

- **renderer** - `crates/forge-tui/src/ui/prompt.rs::render` - draws the orange thick chrome itself; callers don't render a separate block.
- **state** - `crates/forge-tui/src/app/prompt.rs::PromptState` (per-prompt) on `UiSession.prompt_queue: VecDeque<PromptState>` (per-session FIFO).
- **keymap** - `crates/forge-tui/src/app/prompt.rs::handle_key_option_picker` / `handle_key_editing_input`. Top-level dispatch via `dispatch_key` called from `crates/forge-tui/src/app/keys.rs::dispatch_key_by_focus` when the active session has a queued prompt. Notes-kind keystrokes route to `App.input` editor directly.
- **response dispatch** - `crates/forge-tui/src/app/prompt.rs::submit_prompt` / `cancel_prompt` - builds `PermissionOutcome` / `QuestionOutcome` from the picked option's `action` + notes + edited_input, dispatches via `events::turn::dispatch_permission_outcome` / `dispatch_question_outcome`.
- **option construction** - `crates/forge-agent/src/forge_sdk_worker.rs::build_permission_options` - derives contextual options from `ctx.suggestions`; appends "Allow with edits" for editable tools and "Tell Claude something else" universally.
- **chat-input draft preservation** - When a prompt arrives onto an empty queue, `snapshot_draft_if_needed` captures any in-flight chat-input text and clears the editor. When the last prompt resolves (queue empties), `restore_draft_if_empty_queue` puts it back verbatim.
