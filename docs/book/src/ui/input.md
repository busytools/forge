# Input area + autocomplete

## Input area (default state)

The composer box grows from 1 row up to 50 as you type: a thick rust-orange border with no separator rows, a rust-orange `➤` prompt char, dim italic placeholder, slash commands in light magenta.

Above the input a hint slot carries login, cancel and suggestion hints.

<details>
<summary>Hint slot</summary>

A login hint: `Authentication required: <method> -- <description>` in yellow with a dim "Run `claude auth login` in another terminal to authenticate" beneath - two lines. A cancel hint: `Cancelling current turn... draft will auto-submit when ready.` in dim, one line. A prompt suggestion: `Suggestion: <text>    Tab to accept` - dim label, white text, dim accept-hint, one line.

</details>

While the session is connecting the entire input area is replaced with a spinner and "Connecting to Claude Code..." in dim.

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

With `[dictate]` enabled and the models loaded, a take lives entirely inside the composer's interior: idle reserves nothing, recording grows the interior one row, and the row collapses when the take resolves.

<details>
<summary>Status row: anatomy, states, notices</summary>

The status row occupies the same slot the notice row uses, so the two never coexist: a stamped notice keeps the slot and the status row does not render. Level readings arrive every 50 ms, each the peak over the window since the previous one. A long take is cut into segments at measured pause boundaries, and each segment transcribes while the microphone is still recording - the row shows those words as a settled count long before the speaker stops.

<div class="term">

  <pre class="indent">
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>   recording - the interior grows one row; settled segments count on the label
  <span class="accent">┃</span>  <span class="accent">●</span> <span class="accent">0:07</span> <span style="color:rgb(255,176,88)">-18 dB</span> <span class="dim">listening · 2 ready</span> <span class="dim">▁▂</span><span style="color:rgb(171,97,0)">▄▆</span><span style="color:rgb(255,176,88)">█</span><span style="color:rgb(171,97,0)">▅▃▂</span><span class="dim">▁▁</span><span style="color:rgb(171,97,0)">▃▅▆</span><span style="color:rgb(255,176,88)">█</span><span style="color:rgb(171,97,0)">▆▄▂</span><span class="dim">▁▁</span><span style="color:rgb(171,97,0)">▂▄</span><span style="color:rgb(255,176,88)">▆</span><span style="color:rgb(171,97,0)">▄▃▂</span><span class="dim">▁</span>       <span class="dim">esc cancel</span><span class="accent">┃</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                            <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span></pre>

</div>

The row's anatomy is identical across both live states - indicator dot, `m:ss` timer, the live dB figure, label, meter, right-aligned esc hint - and only colour and freeze change on the handoff. While recording: an orange dot `●` pulsing on a 1.05 s cycle (held steady under reduced motion), an orange timer live off the take's own start stamp, a DIM `listening` label that grows a settled-segment count (`listening · 2 ready`) as the pipelined segments settle, and a 26-cell meter. The meter is normalized display-side: gated at the take's own silence floor, then graded dim through orange toward the hot tint rgb(255,176,88) by value - cells at or under the gate draw the floor glyph in DIM, so a bar that never leaves the floor is the same structural silence a no-audio outcome reports. The composer border eases toward the hot tint in proportion to the current level, never more than 35% of the way.

Between the timer and the label rides the live dB figure, such as `-18 dB`: refreshed at 5 Hz whatever the frame rate, its colour following the current level - DIM at or under the gate, grading through orange toward the hot tint above it. The figure lives entirely in the status row, so the draft is never painted over and the caret stays the normal blinking block in every state, recording included. While transcribing the figure holds its last reading, DIM alongside the frozen meter.

<div class="term">

  <pre class="indent">
  <span style="color:rgb(97,160,224)">┏</span><span style="color:rgb(97,160,224)">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span style="color:rgb(97,160,224)">┓</span>   transcribing - same anatomy, frozen and dimmed toward blue; multi-window takes tally the window on the label
  <span style="color:rgb(97,160,224)">┃</span>  <span style="color:rgb(97,160,224)">◌</span> <span class="dim">0:07</span> <span class="dim">-18 dB</span> <span class="dim">transcribing 2/6</span> <span style="color:rgb(38,53,74)">▁▂</span><span style="color:rgb(47,72,109)">▄▆</span><span style="color:rgb(54,85,130)">█</span><span style="color:rgb(47,72,109)">▅▃▂</span><span style="color:rgb(38,53,74)">▁▁</span><span style="color:rgb(47,72,109)">▃▅▆</span><span style="color:rgb(54,85,130)">█</span><span style="color:rgb(47,72,109)">▆▄▂</span><span style="color:rgb(38,53,74)">▁▁</span><span style="color:rgb(47,72,109)">▂▄</span><span style="color:rgb(54,85,130)">▆</span><span style="color:rgb(47,72,109)">▄▃▂</span><span style="color:rgb(38,53,74)">▁</span>   <span class="dim">esc cancel</span><span style="color:rgb(97,160,224)">┃</span>
  <span style="color:rgb(97,160,224)">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                        <span style="color:rgb(97,160,224)">┃</span>
  <span style="color:rgb(97,160,224)">┗</span><span style="color:rgb(97,160,224)">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span style="color:rgb(97,160,224)">┛</span></pre>

</div>

The row renders the moment the phase flips, however brief the transcription - warm takes (roughly 115 ms for a 5-second clip) are visible too. The transcribing row keeps the same anatomy: a blue pulsing dot `◌` rgb(97,160,224), the timer frozen at the take's length and DIM, a DIM `transcribing` label, and the meter frozen at its last recording frame - the cells only change colour, tinting dim toward blue. The border eases toward the same blue the moment the handoff lands. Pipelined segments have been settling since recording began; the stop makes the total known and the label tallies the remainder as progress steps arrive - `transcribing 2/6` counts segments settled against the final total. Single-segment takes never show a tally.

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

**The notice row.** Everything that is not "text landed" is one row inside the box, above the draft, cleared by the next keystroke - the same slot the status row borrows while a take is live, so a notice always wins it. It is the one row that ever changes the box height:

<div class="term">

  <pre class="indent">
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>
  <span class="accent">┃</span>  <span class="dim">nothing above -50 dBFS in 4s · loudest was -38.2 · try again</span>             <span class="accent">┃</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                        <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span></pre>

</div>

Notices: a quiet room carries its own measured peak and offers a retry (DIM); every sample exactly zero is structural and sticky, so no retry is offered (red); a take that normalised to nothing says so (DIM); a truncated take lands its words plus a keep-going note (yellow); recognition failures are one grouped notice (DIM); a busy microphone names the holder, and a device that would not open is refused before recording starts (both red). Landed text inserts at the caret through the editor's own insertion path - never the paste dispatcher, so long takes are never collapsed to a placeholder - and a copy lands on the system clipboard alongside. The take is bound to the session that started it: results route by session key, so switching tabs mid-transcription never moves the words to another composer.

</details>

- <kbd>Esc</kbd> discards a recording and abandons a transcription in flight; it cancels a turn only when no take is live. Recording starts from the [push-to-talk key](./pickers.md).

## Autocomplete dropdown

Open while you type one of four triggers: `/` (slash commands), `@` (files), `&` (subagents), `:` (emoji). A rounded-border dropdown anchored to the input; the title names the mode in dim - ` Commands (N) `, ` /<cmd> Args (N) `, ` Files & Folders `, ` Subagents (N) `, ` Emoji `. Row caps: slash 20, mention 32, emoji 10, subagent 8, clamped to what fits above or below the input, scrolling in place. Each item: a 3-char prefix (` ▸ ` rust orange bold when selected), primary text with the match highlighted, an optional dim description. Slash commands are magenta in the input but default fg inside the dropdown.

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

*Match highlighting renders as a different style - fg colour or background, not strictly underlines.*

<details>
<summary>Command source and known gaps</summary>

The slash list (also feeding `/help`) seeds at connect from the CLI's init and live-refreshes on every `commands_changed` event after a plugin or command reload - a newly added command shows immediately without a restart. Known gaps: the pre-init gap ([#46](https://github.com/busytools/forge/issues/46) - the CLI emits init only post-first-turn) and slash commands rendering as full expanded prompt text on session resume ([#47](https://github.com/busytools/forge/issues/47)).

</details>

### `:shortcode:` emoji picker

Slack-shaped: `:` plus two characters opens it, typing filters, <kbd>Enter</kbd> or <kbd>Tab</kbd> replaces the whole token with the glyph, and a closing `:` on an exact shortcode lands it too - `:tada:` works straight through. <kbd>Esc</kbd> dismisses the picker only. Rows are the glyph then its `:name:` with the match highlighted, in the same rounded box as the other triggers.

Not chat-only: it also serves the /diff inline comment editor and the Finish-review overview, and while open there it captures <kbd>Esc</kbd>, <kbd>Enter</kbd> and the arrows.

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

<details>
<summary>Emoji picker rules</summary>

The `:` counts only at the start of a line or directly after whitespace, and the query must be `[a-z0-9_+-]` - so `http://`, `10:30`, `note:todo` and `Foo::bar` never open a picker (the same rule `@` uses). The table is a curated ~200 GitHub / Slack shortcodes sorted by name (deliberately not the full Unicode set); ranking is exact match, then prefix, then substring, ties alphabetical.

</details>

# Unified prompt (permission · plan · question)

## Dock-morph widget

Permission requests, plan approval and AskUserQuestion route through one widget with three modes (single-select, multi-select, selection+text). When a prompt arrives the input box morphs: the text slot disappears, the orange chrome stays, options appear inside with a `▸` pointer - arrows move, <kbd>Enter</kbd> confirms, <kbd>Esc</kbd> cancels. The tool block above keeps its in-progress render. Prompts queue FIFO per session; with more than one pending a dim "`▼ N more pending after this`" line tops the dock body.

**Common-case permission** (most Bash / Edit / Read calls):

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

**Permission with full CLI context** - Read outside workspace: the yellow `⚠` line is the decision reason, the dim line below the description:

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

**AskUserQuestion** - multi-select with the notes option toggled: a rust-orange `?` on the header, white body below, `[x]` / `[ ]` markers before the icons; toggling `... Tell Claude something else` expands an inline notes editor:

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

<details>
<summary>The dictate blip on the dock</summary>

While a take is live the pulsing circle blip - orange while recording, blue while transcribing, gone when the text lands - leads the dock's footer hint row, a fixed chrome spot whichever option holds focus. The blip is the whole indicator on this surface: no timer, no dB figure, no meter. The first <kbd>Esc</kbd> with a take live abandons the take and the dock stands; the next one rejects it. Dictated words land in the notes draft either way.

</details>

**Queue indicator** (depth > 1, e.g. a background session's prompt queued behind the active one):

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

| Glyph | Meaning | Color |
|---|---|---|
| `✓` | Allow (any Allow variant) | green |
| `✗` | Deny | red |
| `✎` | Allow with edits - synthesized for editable tools (Bash, Edit, Write, MultiEdit, NotebookEdit) | blue |
| `...` | "Tell Claude something else", the synthesized escape hatch, always last | dim |

<details>
<summary>Where options come from</summary>

Options derive from the CLI's `permission_suggestions`: `addRules` (Read outside workspace) offers "Allow always for {tool} · paths matching {pattern}" - macOS `/tmp` + `/private/tmp` mirror entries are deduped for display, both rule entries kept on the wire; `addDirectories` (Write / Edit outside workspace) offers "Allow always & add {dirs} to allowed dirs"; `setMode` (typically the prompt that intercepts plan-mode) offers "Allow always & switch to {mode}" - ExitPlanMode itself sends `permission_suggestions: null`, so the mode switch lives on the earlier intercept prompt, not on ExitPlanMode. Two options are synthesized universally: `... Tell Claude something else` (always last) routes to deny with the notes text, and `✎ Allow with edits` (editable tools only) routes to allow-with-input. The Notes row's `[x]` is display-only - it tracks the live notes buffer, and the wire `annotation.notes` field carries the typed text independently of `selected_option_indices`.

</details>

<details>
<summary>Keymap and draft preservation</summary>

`↑` / `↓` / `←` / `→` move option focus (wrapping top to bottom); `Home` / `End` jump to first / last; `Space` toggles in multi-select; <kbd>Enter</kbd> submits - except on the Notes option, where it submits the notes text, and on the Edit option, where it enters an editing input; <kbd>Esc</kbd> cancels; `Tab` / `BackTab` / printable characters / `Backspace` / `Delete` route to the notes editor when the Notes option is focused and are swallowed otherwise.

When a prompt arrives onto an empty queue, any in-flight chat draft is captured and cleared; when the last prompt resolves, the draft is restored verbatim.

</details>
