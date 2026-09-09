# Chat - message types

Every message in the scrollback belongs to one role. Roles render with different visual treatments today.

Hovering the chat shows an I-beam pointer over selectable text and a hand over clickable blocks (tool calls, group headers, the scrollbar, pane rows), so you can tell where a drag will select versus toggle. The shape is the OS pointer set via `OSC 22` (forge enables any-motion mouse tracking to drive it); it is not rendered by forge, so hovering never triggers a redraw. forge emits the default (arrow) shape once at startup, so the pointer is correct over chrome before the first hover instead of showing the terminal's own text-surface I-beam.

## User message

*visible: whenever the user has sent a turn*

Role banner is the literal text "**User**" in `theme::DIM` bold (not the user's name, and not in RUST_ORANGE). Body block is rendered as a markdown text block with `USER_MSG_BG` applied as a per-cell background that extends to roughly the right edge.

<div class="term">

  <pre class="indent">
  <span class="dim bold">User</span>
  <span class="user-band">  Read the rate-limit code and add a softer wording branch.                </span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/message.rs` · banner via `role_label_line(MessageRole::User)` · body via `append_user_blocks` with `text_block_layout(_, _, Some(theme::USER_MSG_BG), true)`
- **color** - banner: `theme::DIM` + BOLD  ·  body bg: `theme::USER_MSG_BG` (`Rgb(40, 44, 52)`)  ·  body text: terminal default fg

## Assistant message

*visible: every assistant turn*

An assistant turn has no header row: the body starts on the first line. A collapsible **turn info** row trails the body as the last row, in the same shape as the stop-hook summary chip directly above it: one glyph at column 0, `·`-separated fields, all DIM, and a bracketed `[▶ expand]` toggle. That leading glyph is the **spinner while the turn runs** and `↳` once it settles, so the row is the turn's live indicator on the way to being its footer. Body is full markdown via `tui-markdown` (which uses `pulldown-cmark`) with `syntect` for fenced code blocks.

The row does not appear when the turn ends. It appears when the turn *starts* and counts up, so by the time it settles the user has been watching it and it reads as the turn's own footer rather than an appendix. Elapsed ticks; the thinking estimate and the input and cache tokens accumulate as each API call lands. Output tokens, cost and the API/local split do not exist until the `Message::Result` frame, so the collapsed row omits them and the expanded one dashes them, rather than showing a zero.

One row per turn while the turn runs on. A prompt submitted mid-turn with no cancel in flight rides the running turn rather than starting one (steering): the submit carries the running row onto the fresh tail placeholder with its clock intact and the message that was streaming sheds it. A prompt submitted over a pending cancel starts the row's clock over instead, matching the interrupted turn's restart. A delivered turn (peer, worker, cron, gotify) stamps its clock at the turn-open instead of at its first usage-bearing frame, so the row never sits as a bare loader while it waits.

It carries no `turn info` label. The row is the only thing at the foot of an assistant message wearing this shape, so the words identified nothing the position did not, and the twelve columns they cost pushed the drop thresholds below wider terminals than they had to.

**A running row is allowed to sit alone; a settled one is not.** Before any body content exists the row is all there is to paint, and it is the only indication the turn is alive. Once the turn settles the old rule applies again: a turn whose body rendered nothing visible gets no row, because there is nothing left to hang it under and a lone footer under an empty message reads as a bug. A row with nothing stamped on it yet says only that the turn is alive, so it earns its line only while nothing else on the message says so and the session's turn clock is actually running (a Result settles the clock, so post-turn traffic cannot revive the row) - during a compaction the compacting line is already saying it, and a second spinner carrying no figures would be noise.

**Once the row has figures a compaction does show two spinners**, and that is deliberate. The compacting line says what is happening; the row says how long the turn has been going and what it has spent, which the compacting line does not carry. That spot already showed two rows before this change - one spinner and one arrow - so only the second glyph is new:

<div class="term">

  <pre class="indent">
  I'll fold the earlier context down before carrying on.

  <span class="accent">&#x280b; Compacting context...</span>
  <span class="dim">&#x280b; 4m 42s &#xb7; thinking 434 &#xb7; 45&#x2191; &#xb7; 95% cached &#xb7; 135k written [&#x25b6; expand]</span></pre>

</div>

The count-up needs the turn to own its message. A turn reusing an unsettled one - a live tail whose pointer was lost, or a wire user prompt, which opens no placeholder of its own - shares the row until its Result replaces the fields, and both live writers skip a settled row, so the reuse never lands on finished figures. A tool_use opening after a settled row starts its own message rather than gluing in, and a Result racing a mid-turn submit settles on the body row the submit shed its bar from, so turn exit's strip of the still-empty placeholder cannot take the figures with it. A resumed session renders no turn-info row at all: replay synthesises only assistant and user messages, so no Result reaches the reducer and neither half of the row is ever known.

**Collapsed** is the default, and the expanded flag lives on the `ChatMessage` so it survives a re-render. **While the turn runs** the token field carries only its input half, since there is no honest output count yet, and under a minute the elapsed always ends in `.0` - not because it is unsettled, but because a running row ticks in whole seconds and `format_turn_duration` shows tenths for anything under 60 s. The **thinking** estimate sits at position 2, and is a running-row field only:

<div class="term">

  <pre class="indent">
  <span class="dim">&#x280b; 12.0s &#xb7; thinking 434 &#xb7; 4.2k&#x2191; &#xb7; 93% cached &#xb7; 3.1k written [&#x25b6; expand]</span></pre>

</div>

Settled, the spinner becomes `↳`, the output half of the token pair arrives, and **thinking** drops out - once real billed counts exist, an estimate of reasoning the user is not charged for is no longer the most useful thing in that width. It stays visible in the expanded body, where the space is not contested:

<div class="term">

  <pre class="indent">
  <span class="dim">&#x21b3; 1m 19s &#xb7; 4.2k&#x2191; 1.1k&#x2193; &#xb7; 93% cached &#xb7; 3.1k written [&#x25b6; expand]</span></pre>

</div>

**Expanded** replaces the toggle label and adds an indented two-column body. An unknown cell renders `-`. A zero the wire actually reported is a measurement and renders as `0`:

<div class="term">

  <pre class="indent">
  Tests pass. Want me to push?
  <span class="dim">&#x21b3; 1m 19s &#xb7; 4.2k&#x2191; 1.1k&#x2193; &#xb7; 93% cached &#xb7; 3.1k written [&#x25bc; collapse]</span>
  <span class="dim">    ended     15:48:31        model   claude-opus-5</span>
  <span class="dim">    elapsed   1m 19s          api     1m 04s</span>
  <span class="dim">    local     15.0s tools + hooks</span>
  <span class="dim">    thinking  434 est</span>
  <span class="dim"></span>
  <span class="dim">    in        4,231           out     1,102</span>
  <span class="dim">    cache     108,442 read    wrote   3,180</span>
  <span class="dim">              93% of input served from cache</span>
  <span class="dim"></span>
  <span class="dim">    session   $4.82 cumulative</span></pre>

</div>

**The body holds its height across the settle.** Open it on a running turn and it does not grow a row under the cursor when the Result lands - `local`, `thinking` and `session` are each held with a `-` rather than dropping out, so the same ten lines are there before and after. Each holds as a bare dash: `local` without its `tools + hooks` tail and `session` without the `$` and the word `cumulative`, neither of which means anything attached to nothing:

<div class="term">

  <pre class="indent">
  <span class="dim">&#x280b; 12.0s &#xb7; thinking 434 &#xb7; 4.2k&#x2191; &#xb7; 93% cached &#xb7; 3.1k written [&#x25bc; collapse]</span>
  <span class="dim">    ended     -               model   claude-opus-5</span>
  <span class="dim">    elapsed   12.0s           api     -</span>
  <span class="dim">    local     -</span>
  <span class="dim">    thinking  434 est</span>
  <span class="dim"></span>
  <span class="dim">    in        4,231           out     -</span>
  <span class="dim">    cache     108,442 read    wrote   3,180</span>
  <span class="dim">              93% of input served from cache</span>
  <span class="dim"></span>
  <span class="dim">    session   -</span></pre>

</div>

**The cache-percentage line is the deliberate exception** and stays conditional. It is the one row that is a sentence rather than a labelled cell, and `- % of input served from cache` reads as broken rather than pending. It appears as soon as a cache read is known, which is the turn's first assistant frame, so in practice it is present for almost all of the running window and its absence costs a line only at the very start.

<div class="term">

  <pre class="indent">
  Looking at <span class="dim">crates/forge-tui/src/app/events/rate_limit.rs</span> - the warning chip
  has only one wording today. I'll add a quieter branch for the
  near-threshold-no-overage case.

  Here's the function:

      <span class="dim">fn is_near_threshold_without_overage(</span>
      <span class="dim">    update: &amp;model::RateLimitUpdate,</span>
      <span class="dim">) -&gt; bool {</span>
      <span class="dim">    matches!(update.status, RateLimitStatus::AllowedWarning)</span>
      <span class="dim">        &amp;&amp; update.is_using_overage == Some(false)</span>
      <span class="dim">        &amp;&amp; update.surpassed_threshold.is_some_and(|t| t &gt; 0.0)</span>
      <span class="dim">}</span>

  Tests pass. Want me to push?
  <span class="dim">&#x21b3; 1m 19s &#xb7; 4.2k&#x2191; 1.1k&#x2193; &#xb7; 93% cached &#xb7; 3.1k written [&#x25b6; expand]</span></pre>

</div>

- **code** - `role_label_line` in `crates/forge-tui/src/ui/message.rs` returns `None` for Assistant, so no header row is pushed; `append_turn_info` appends the trailing row from `build_message_layout` · markdown via `crates/forge-tui/src/ui/markdown.rs`
- **color** - every span on the row and its expanded body: `theme::DIM`  ·  body: terminal default fg with markdown styles  ·  inline code: `theme::DIM` tint
- **order** - body → stop-hook summary chip → turn info → separator. The row carries no leading blank, so it reads as a footer on the message rather than floating between two; the unconditional trailing separator still follows it, so messages stay one blank line apart and the row does not double it.
- **duration formatting** - `format_turn_duration`, shared with the expanded stop-hook rows: < 60s → `12.4s` (one decimal) · >= 60s → `1m 04s` · >= 1h → `1h 02m 04s`. Token counts use `format_token_count_short` (`4.2k`, `1.4M`) collapsed and full `1,102` grouping expanded.
- **truncation** - The drop order is **stated, not positional**: `written` first, then `cached`, then `thinking`, then the token pair. It has to be stated because `thinking` renders at position 2 but must shed *before* the token pair at position 3 - a right-to-left walk would shed real billed usage to keep an estimate, which is backwards. Each step is otherwise the field least likely to change what the reader does next. The glyph, the elapsed time and the toggle are never dropped; below even that width the row wraps like any other line. Dropping the `turn info` label freed roughly twelve columns, so every threshold moved: the row now survives to a terminal about twelve columns narrower before it sheds its first field.
- **field scope** - The wire mixes per-turn and session-cumulative fields, so the row cannot render `Message::Result` verbatim. `duration_ms` and `usage` are per turn. `duration_api_ms` is **session-cumulative**, so the turn's API time is its delta against the previous Result in that session; the naive `duration_ms - duration_api_ms` goes negative from the second turn onward. `local` is what is left after subtracting that delta, and is suppressed rather than clamped when the delta exceeds wall clock (concurrent subagent calls) or when the counter resets after a compaction. `total_cost_usd` is session-cumulative and is labelled `session` for that reason. `num_turns` is *not* the session's turn count despite its doc comment - it counts agentic iterations within the one request - so it is not rendered.
- **missing values** - `usage` and `total_cost_usd` are both `Option` on the wire, and an absence is never rendered as `0`, because a zero is a claim and an absence is not. How it reads depends on the cell: an absent count drops out of the collapsed row and renders `-` in the expanded one, including the three rows that can be absent while the rest are present - `local`, `thinking` and `session` hold their line with a dash so the body keeps its height across the settle. `local` and `session` fill only when the Result lands; `thinking` fills during the turn if it fires at all, and stays a dash for a turn that never thought. The cache percentage is the single row that still drops rather than dash, for the reason given above. A zero the wire did report is a measurement and renders as `0`.
- **zero is not a measurement** - A present value can still be no information. A `duration_api_ms` resolving to zero means the CLI attributed no API time, not that the turn was instant - the counter is millisecond-granular, so a turn that reached the API cannot register zero. An all-zero `usage` block says the same about tokens. Neither reaches the row: the `api` and `local` cells both render `-`, while the token cells keep whatever the turn's own assistant frames established - which on an interrupted turn are real running counts, so clearing them would suppress a true measurement to fix a false one. The rule keys on the **whole** usage block: a lone zero inside a real one, a turn that wrote nothing new to the cache, is a measurement and renders as `0`.
- **a row never mixes two Results** - A Result can reach a message that is not its own when a compaction emits one with no assistant message at all. The row must still describe a single Result either way, so the rule is about **coherence, not precedence**: a Result carrying usage overwrites every accounting field together and is allowed through even onto a settled row, while one with no usable token counts is refused by a settled row, because it cannot replace the counts already there and its clock would sit over another turn's figures. The refusal is keyed on the settled row, not on the compaction: a compaction whose Result arrives after the previous turn settled - the ordinary case, and the one `compact.jsonl` captures - renders no row, which is the one shape left where a turn has no row of its own; one reaching a row that is still live does stamp its clock there, which is a real wall clock and the only turn it can belong to.
- **cache percentage** - `cache_read_input_tokens` over `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`. Both cache counters are **input** tokens - one written into the cache at a premium, one read back cheaply - so there is no output cache and neither is ever labelled as output. The expanded row names the denominator inline.
- **thinking estimate** - Summed from `estimated_tokens_delta` on `system/thinking_tokens`, not read off `estimated_tokens`. **Both of that event's counters are documented as per-turn and both are per *thinking block***: measured across the 2.1.220 baselines, `exit_plan_mode` runs 50, 164 and then restarts at 50, 150, 250, 270 inside a single Result, and `permission_request_hook` does the same at 50, 71 then 50, 150. The restart carries `delta=50` rather than a negative step, so summing every delta is the exact per-turn total with no boundary to detect - verified equal to the sum of the per-block finals on all nine baselines that carry the event, with no negative delta anywhere. The last estimate happens to equal the largest one on all nine, which is why reading the raw counter looked right and was true for the wrong reason. It is labelled `est` because it is the CLI's estimate of reasoning tokens, is not billed, and sits in the body rather than beside the billed counts for that reason. A turn that fired no such event renders `-`.
- **stamp source** - Settled fields are stamped from `Message::Result` by `stamp_turn_info_on_latest_assistant` in `app/events/sdk_message.rs`, which invalidates the layout as well as the render cache because the row changes the message's height. When the Result finds the tail placeholder a mid-turn submit opened still empty, the stamp diverts to the nearest earlier unsettled body row, so the turn exit's strip of that placeholder cannot take the figures with it. The end time is stamped locally on arrival: wall-clock is not on the wire. Running fields come from `AssistantEnvelope.usage`, deduplicated on `message.id` because the CLI splits one assistant message across a frame per content block and repeats the usage on each. `output_tokens` on those frames is the streaming `message_start` placeholder, not a count, so it is ignored until the Result lands. `system/turn_duration` never fires in 2.1.156 (verified across 43 baselines + 14 fresh captures, all zero); the prior chip read from that dead event and was deleted in #283. The thinking estimate accumulates on a session-scoped field which reaches the message through **four** writers, three of them assigning the field and the fourth replacing the whole `TurnInfo`: the thinking event (`mirror_thinking_tokens_onto_turn`), the live usage stamp (`record_live_turn_usage`), the settle (`stamp_turn_info_on_latest_assistant`) and turn start (`App::start_live_turn`). Counting only the first three is what hid a defect where an interrupted turn's estimate was added to the next turn's, so the count is established by a grep that matches whole-struct assignment as well as field assignment - `grep -rnE "turn_info\s*=|thinking_tokens\s*="` - rather than by the field-assignment pattern that names three.

  The event can only ever write a number, so it is the usage stamp and the settle that **assign unconditionally**, which stops a turn reusing an unsettled row from inheriting the previous turn's estimate - a real sequence, since a plain user prompt opens no placeholder of its own. Turn start covers the other path, resetting the row and the accumulator together, because the deltas accumulate and a stale accumulator would be added to rather than replaced. The two turn-boundary clears on the session field stay load-bearing, and the settle reads it before clearing it.
- **ticking** - No new timer. While a turn runs the status is `Thinking` or `Running`, which already forces a repaint once per `repaint_interval`; the row's cache key carries elapsed *whole seconds*, so the *elapsed* moves that key only on a second boundary.

  **That is not how often the message rebuilds, and the difference matters for an open row.** The same signature folds the spinner glyph whenever the message is a running assistant, and the glyph turns over every 32ms on the default braille style - so a live turn's message re-lays out around 31 times a second, not once. Measured at roughly 28µs collapsed against 148µs expanded for a one-paragraph turn, an open body therefore costs about 120µs per rebuild and 3.75ms of work per second, which is 22.5% of one 60fps frame budget spread across that second, on the one message that is running. Still worth having the row expandable while it counts rather than only after it settles, but it is near a quarter of a frame rather than the hundredth a once-a-second rebuild would have cost.
- **expand** - Click the row to toggle, same as the stop-hook summary chip and with the same hit-test shape plus a measured-width guard. It is in `pointer_shape_at`'s clickable set too, so it hovers as a hand rather than an I-beam. **Cmd+X** (Ctrl+X off macOS) reaches it through the existing toggle-all, which clears every per-row override in the active session so anything clicked open or shut returns to what the flipped flag now dictates - the expanded flag is per-row state of exactly that kind, so a turn info row clicked open snaps shut alongside a tool call clicked open. No new mechanism and no binding of its own. **The symmetry is one-way**: tool calls have an app-global `tools_collapsed` to return to, and the row has none, so expand-all opens the tool calls and still shuts the row. Collapsing is what the shortcut is reached for, and giving the row a global of its own was not worth a second flag.

## Compacting indicator

*visible: on the active turn's assistant message, while the session is compacting*

The first line of the active assistant message's status slot: `⠋ Compacting context...`, the leading glyph in the session's active spinner frame and the whole line in RUST_ORANGE - the only one of the slot's lines that is not `DIM`. It trails the message body, after a blank line when the body has content; on a body-less placeholder it is the whole body. Arming is wire-driven, never optimistic: `status:"compacting"` arrives through `apply_session_status_update` and each typed `compact_boundary` re-arms it via `handle_compaction_boundary_update` (recording the trigger and pre-tokens); it clears when the CLI reports the settle (`status:null`), which is also when a manual `/compact` emits its success notice.

<div class="term">

  <pre class="indent">
  <span class="accent">&#x280b; Compacting context...</span></pre>

</div>

**It does not replace the turn info row.** A turn info row that already has figures still renders beneath it, the two-spinner case shown under **Assistant message**.

- **code** - `crates/forge-tui/src/ui/message.rs::compacting_line`, pushed from `append_assistant_blocks` (body-less message) and `build_message_layout` (after the body); the active-turn restriction rides the `msg_spinner` gate, like the thinking flags
- **color** - glyph + text: `RUST_ORANGE`
- **data source** - `App::is_compacting`, driven by the CLI's session-status stream (`status:"compacting"`) and `compact_boundary` events; the glyph frame is the shared spinner (see [\[ui\] spinner](./pickers.md))

## Stop-hook summary (end-of-turn)

*visible: end of an assistant turn whose `system/stop_hook_summary` event reports `actions > 0`*

Optional collapsed-by-default 1-liner appended to the end of an assistant turn when one or more `Stop`-event hooks executed during the turn. Click the `[▶ expand]` affordance (or keyboard toggle) to expand the summary text inline. When `actions == 0` the surface is hidden entirely (no row reserved). Sourced from `system/stop_hook_summary` (CLI 2.1.156+).

Collapsed:

<div class="term">

  <pre class="indent">
  <span class="dim">↳ hook summary · 3 actions [▶ expand]</span></pre>

</div>

Expanded:

<div class="term">

  <pre class="indent">
  <span class="dim">↳ hook summary · 3 actions [▼ collapse]</span>
  <span class="dim">    Posted to Slack #ops · attached transcript.</span>
  <span class="dim">    Wrote /tmp/forge-handoff.log.</span>
  <span class="dim">    Bumped #142 to Done in Linear.</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/message.rs` + per-message expand-state on `UiSession` (matches existing tool-card collapse pattern)
- **color** - line + body: `theme::DIM`

## System notice

*visible: rate-limit warnings, mode changes, parse errors, retries, slash-command output, connection failures*

Banner is the literal severity word "**Info**" / "**Warning**" / "**Error**" in bold, colored by severity (Info=DIM, Warning=STATUS_WARNING, Error=STATUS_ERROR). Body lines are tinted with the same color via `tint_lines` after markdown rendering. Hard wraps at the terminal width.

**Placement:** anchored to where the event happened. While a turn is in flight (status Thinking / Running) the notice inserts just above the active assistant placeholder, so a mid-turn notice (e.g. a worker-closed toast) flows inline and scrolls up with the conversation instead of stranding at the bottom below the still-streaming turn - the turn pointer shifts with the placeholder so the spinner stays put and the response keeps streaming there. When idle, or at turn end (an error / completion message), it appends at the tail. A rate-limit warning reaches the same inline position through the turn-notice path.

<div class="term">

  <pre class="indent">
  <span class="warning bold">Warning</span>
  <span class="warning">Near rate-limit threshold. Resets in 4h 23m at 14:30 UTC.</span>

  <span class="error bold">Error</span>
  <span class="error">Failed to parse ~/.claude/settings.json: expected `,` at line 42 column 5. Falling back to defaults.</span>

  <span class="dim bold">Info</span>
  <span class="dim">Mode changed: default → acceptEdits</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/message.rs::system_role_label_line` + `notice_block_layout` + `tint_lines`
- **placement** - `crates/forge-tui/src/app/events.rs::insert_active_system_message` anchors above the in-flight assistant while running (via `insert_message_tracked`), tail otherwise
- **severities** - `SystemSeverity::Info` → `theme::DIM` · `Warning` → `theme::STATUS_WARNING` · `Error` → `theme::STATUS_ERROR`

# Tool calls

Every tool invocation renders through the same path: a single title row at column 2 (status icon + kind icon + display title) followed, when there's body content, by lines prefixed with `  │  ` (DIM). No bordered cards - Bash, Read, Edit, Grep, etc. all share one shape. The body content varies by tool kind (raw terminal output for Bash, syntax-highlighted code for Read, unified diff for Edit / Write / MultiEdit, etc.).

## Standard row (Read · Write · Edit · Grep · Glob · etc.)

*visible: any tool call that isn't Bash / TodoWrite / Subagent*

Single line: 2-space indent + status icon (success / fail / spinner) in status color + space + tool kind icon (white bold) + space + **kind label** (white bold - "Read", "Edit", "Bash", etc. from `theme::tool_name_label`) + space + display title (markdown inline spans, default fg). Body lines (when present) prefix each with "`  │  `" dim (5 chars: 2 + box-drawing-vertical + 2), last line with "`  └─ `" dim (5 chars). If `tc.title` from claude already starts with the kind label (e.g. claude often sends "Read /path/to/file.rs"), the duplicate prefix is stripped so the column reads cleanly.

**Collapsed** (when collapse is in effect and the tool call has no diff / pending permission / pending question): the body collapses to a single summary line "`  └─ <content_summary>  click or ctrl+x to expand`" (all DIM). The summary text comes from `content_summary` - last non-empty line of terminal output (capped at 80 chars), or the file name for a Diff, or an MCP resource path, etc.

**Collapse precedence (unified resolver):** every render-time "should this be collapsed?" decision routes through `crate::ui::collapse`. Two pure resolvers: `resolve_collapsed_bool(per_item, global_collapsed)` for 2-state items (loose tool-calls, peer/MCP blocks inbound + outbound) and `resolve_group_level(per_group, global_collapsed)` for 3-state items (tool-call groups, messaging groups) where absent + global-collapsed returns `L2Summary` and absent + global-expanded returns `L0Bodies`. Per-item state (the override fields) wins when present; absent falls through to `app.tools_collapsed`. The carve-out predicate `is_carved_out_from_global_directive(tc)` names the kinds that bypass the global directive entirely - Execute / Bash, diff content (Edit/Write/MultiEdit/NotebookEdit), and any tool that actually renders as a lifecycle block (`renders_as_lifecycle_block`, currently Monitor with parseable input) - those render expanded regardless. Keyed on the render rather than the tool name, so a Monitor whose input does not parse paints an ordinary card and stays collapsible.

Ctrl+X (Cmd+X) is a **binary global toggle**, not a graduated cycle. Press 1 expands every non-carved-out kind (groups to `L0Bodies`, tools / peer / MCP to expanded); press 2 collapses everything (groups to `L2Summary`, tools / peer / MCP to collapsed). The gesture also clears every per-item override (`tc.collapsed_override`, `text.peer_collapsed_override`, `group_collapse_levels`, `messaging_group_collapse_levels`) so any row the user had clicked open or closed resets to its default-render state. The intermediate `L1Titles` group level stays reachable only via mouse-click on a group summary row (the per-group L2 → L1 → L0 click cycle is unchanged). At session start the global directive defaults to COLLAPSED, so a fresh chat opens compact.

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">⬚</span> <span class="bold">Read</span> /path/to/file.rs
  <span class="success">✓</span> <span class="bold">▣</span> <span class="bold">Edit</span> /path/to/file.rs (+34, -2)
  <span class="success">✓</span> <span class="bold">▣</span> <span class="bold">Write</span> /path/to/new_file.rs
  <span class="success">✓</span> <span class="bold">▣</span> <span class="bold">Write</span> .claude/plans/launch.md (+45, -2)
  <span class="success">✓</span> <span class="bold">⌕</span> <span class="bold">Grep</span> "is_using_overage"
  <span class="success">✓</span> <span class="bold">⌕</span> <span class="bold">Glob</span> crates/**/Cargo.toml
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Bash</span> cargo nextest run -p forge-tui
  <span class="success">✓</span> <span class="bold">⊕</span> <span class="bold">WebFetch</span> https://docs.rs/ratatui/latest/ratatui/
  <span class="success">✓</span> <span class="bold">⌖</span> <span class="bold">ToolSearch</span> select:CronList
  <span class="success">✓</span> <span class="bold">⊕</span> <span class="bold">WebSearch</span> rust async runtime comparison
  <span class="success">✓</span> <span class="bold">✦</span> <span class="bold">Advisor</span> how to handle a stuck migration
  <span class="error">✗</span> <span class="bold">⬚</span> <span class="bold">Read</span> /path/to/missing.rs</pre>

</div>

**Server-side tool variants** (`ServerToolUse` wire blocks - ToolSearch, web_search, web_fetch, advisor, plus the code-execution family) render through the same standard path. The wire `name` field is the discriminator (`tool_search_tool_regex` / `tool_search_tool_bm25` / `web_search` / `web_fetch` / `advisor`), and both the title formatter and the kind label map those lowercase wire names back to the familiar capitalised form so the card chrome is identical to the in-process equivalents. Result blocks land via the `advisor_tool_result` typed arm (advisor) or via the `is_tool_result_block_type` unknown-arm passthrough (web/tool_search results).

**ToolSearch has a second wire shape forge also renders.** Alongside the server `ServerToolUse` path above, an agent can call ToolSearch itself as a CLIENT `tool_use` (plain `tool_use` block, wire name literal `"ToolSearch"`, input `{query, max_results}`) - this is the deferred-tools tool a forge agent invokes when it doesn't have a tool's schema loaded yet. The title arm in `tool_title` pairs the client name with the server discriminators in one branch (`"ToolSearch" | "tool_search_tool_regex" | "tool_search_tool_bm25"`) so both shapes surface the query as `⌖ ToolSearch <query>`. The matching `tool_result` wire shape is an array of `{type: "tool_reference", tool_name: "..."}` blocks - `build_tool_result_fields` walks the array and synthesises a compact `Found <A>, <B>` body line, NOT the raw `<functions>` schema dump.

- **code** - `crates/forge-tui/src/ui/tool_call/standard.rs::render_tool_call_title`
- **icons + labels** - per `theme::tool_name_label`: Read (⬚) · Write / Edit / MultiEdit / NotebookEdit / Delete (▣) · Grep / Glob / LS (⌕) · Bash (▶) · WebFetch / WebSearch (⊕) · Move / EnterWorktree (⇄) · ExitPlanMode / Config (⊙) · TodoWrite (◌) · Task / Agent → "Subagent" (◇) · ToolSearch (⌖) · Skill / Advisor (✦) · fallback "Tool" (○). The server-tool wire-name variants (`tool_search_tool_regex` / `tool_search_tool_bm25` / `web_search` / `web_fetch` / `advisor`) map to the same glyph + label so the in-process and server-side calls look identical.
- **duplication strip** - if `tc.title` starts with "`<kind> `" (kind label + space), the prefix is stripped so we don't render "Read Read /path"
- **plan-mode aliases** - when `current_mode_id == "plan"`: Write title becomes "Create Plan", Edit/MultiEdit becomes "Update Plan". The kind label still renders ("Write Create Plan" / "Edit Update Plan") - the alias replaces the path, the kind label still names the tool.

## Bash row

*visible: every Bash tool call*

Same standard tool-call shape - status icon + `▶` kind icon (white bold) + the command as the title. Body lines prefixed with `  │  ` (DIM) carry the terminal output through `render_terminal_output` (ANSI handling, output cap at `TERMINAL_MAX_LINES = 12`). Last line uses `  └─ `. No bordered card - Bash flows through the same path every other tool uses.

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Bash</span> cargo nextest run -p forge-tui events::rate_limit
  <span class="dim">│  </span>$ cargo nextest run -p forge-tui events::rate_limit
  <span class="dim">│  </span>   <span class="dim">Compiling forge-primitives v0.14.2</span>
  <span class="dim">│  </span>   <span class="dim">Compiling forge-tui v0.14.2</span>
  <span class="dim">└─ </span><span class="success">Summary [4.197s] 5 tests run: 5 passed, 1174 skipped</span></pre>

</div>

- **code** - title via `standard::render_tool_call_title`, body via `standard::render_tool_content` (which dispatches to `render_terminal_output` when `tc.is_execute_tool()`). When the run failed, only the first non-empty stderr line is shown via `failed_execute_first_line`.
- **icon** - kind icon `▶` (`U+25B6`) in white bold (matches every other tool icon's coloring). Replaces the previous chevron `⟩` (the lone directional outlier).
- **collapse** - same Ctrl+X / click behaviour as other tools - collapses to the single-line summary `  └─ <last output line>  click or ctrl+x to expand` in DIM.
- **resolves** - [#39](https://github.com/busytools/forge/issues/39) - Bash no longer has its own bordered card. Same shape as Edit / Read / Grep. `render_execute_with_borders` deleted entirely.

## Chat tool-call grouping (L2 / L1 / L0)

*visible: any assistant message that produced one or more consecutive tool calls outside the special-render set (edits / writes / monitor / peer block)*

Any consecutive tool calls between two user messages fold into one foldable header. The membership is render-class deny-list: a tool gets its own visible card only when it renders bespoke chat surface. **Run-breakers (always inline, never folded)**: Edit / Write / MultiEdit / NotebookEdit (inline diff view), Monitor (lifecycle block - and because it renders visibly it SPLITS a run rather than passing through it, so `Read,Read,Monitor,Read,Read` reads as two groups of two rather than one of four, and a Monitor between two peer envelopes drops that pair below the 2-envelope messaging-group threshold), and the peer block tools (`peers__ask_agent`, `peers__tell_agent`, `workers__ask`, `workers__tell`). Chat-suppressed tools (Task* / AskUserQuestion / Workflow / Schedule* / Cron*) render nothing visible and pass through the run, so adjacent visible groups merge across them. Everything else folds, including `WebSearch` / `WebFetch` / `LSP` / plain `mcp__*` calls that used to render as standalone cards.

The threshold is 1: a single groupable call also wears the foldable header and renders the same default **L2** summary line as a multi-call group. Per-group cycling is bound to mouse-click on a group's summary row, walking that group's level through L2 (summary) → L1 (title row) → L0 (title + body); single-item groups follow the same path so the chat surface stays uniform regardless of run length. ctrl+x is the session-wide tools-collapsed toggle, not a per-group cycle.

**L2 summary** renders ONE consistent tree shape for every run - single-kind or many. The parent line is the count (`<status_icon> <N> tool calls   ctrl+x to expand`, no box corner); below it one child per kind carries the projects-pane tree connectors - `├─` for each kind, `└─` for the last - plus the `│` spine, so chat and the side panes read as one tree system. A single-kind run is just a one-child tree (there is no more `=` one-liner). The 2-space LEFT indent aligns the leading icon with every other tool row; the connectors sit at column 2. The leading **status_icon** aggregates over the run (animated braille spinner while any call runs, green check when all complete, red cross if any failed, hollow circle when only pending). Glyph + label render BOLD; connectors, spine, targets and the `ctrl+x to expand` hint render DIM. The block carries a leading blank line so it reads as a separate message when adjacent to peer/MCP messages.

**Per-kind children**: each kind is keyed by the glyph `theme::tool_name_label` assigns, so same-glyph tools merge - Grep / Glob / **LS** collapse to one `search` child, WebFetch / WebSearch to one `web`, LSP to `lsp`, and so on. No generic `calls` grab-bag. **Every kind follows one rule**: any kind with resolved targets **nests one child row per target** (uncapped - every resolved target gets a row, like read), whatever its call count, so every detail sits in the same column; a target-less kind shows a bare row, with `×N` when called more than once. **Nothing wraps** - each row is a single line, and the nested target rows are the only ones that clip. Kind rows and the `×N` row render at their natural width, so a long kind label overflows at any width - `├─ ◈ plugin_context7_context7` is 31 cells. Separately, the target budget floors at 8 cells, so below a render width of 16 a child row overflows too; that floor is deliberate, since something useful should render however narrow the pane gets. The outer layout char-wraps *without* the tree gutter, so either of those shears the tree. The parent count row is the exception - it is never clipped and is often the widest row, but it wraps harmlessly because its connectors live on the rows below it. The spine holds `│` while a later kind follows, blank on the last. Kinds render in first-appearance order.

**Read vs the rest (clip style)**: read relativizes each path against the project root (the session's `cwd_raw`) and clips with a **middle-ellipsis** (`crates/.../message.rs`) so the filename stays visible; every other kind clips **end-first** with `...` (keeping the head - the command name / domain / pattern start). `bash` shows Claude's human-readable `description`, `web` the URL (scheme stripped) or query, `toolsearch` the query, `skill` the invoked skill name (+ its `args` when the call carries any), `SendMessage` the recipient + summary (falling back to the full message), `Delete` / `Move` their paths, `LSP` the operation + file, `PushNotification` the message. **MCP by server**: every `mcp__<server>__<tool>` call keys by its server under the distinct `◈` marker (label = server name, target = the tool sub-name), nesting one child per call like every other kind. Peer / worker MCP tools stay run-breakers (they render peer blocks) and never fold here.

Multi-kind run (the tree - every kind nests one row per instance):

<div class="term">

  <pre class="indent">
  <span class="accent bold">⠋</span> <span class="bold">9 tool calls</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">├─ </span><span class="bold">⬚ read</span>
  <span class="dim">│  ├─ crates/forge-tui/src/ui/message.rs</span>
  <span class="dim">│  └─ crates/forge-agent/src/env/git_diff.rs</span>
  <span class="dim">├─ </span><span class="bold">⌕ search</span>
  <span class="dim">│  ├─ render_group_summary</span>
  <span class="dim">│  └─ KindLine</span>
  <span class="dim">├─ </span><span class="bold">▶ bash</span>
  <span class="dim">│  ├─ cargo nextest run -p forge-tui</span>
  <span class="dim">│  ├─ just check</span>
  <span class="dim">│  ├─ git status --short</span>
  <span class="dim">│  └─ gh pr create --fill</span>
  <span class="dim">└─ </span><span class="bold">⊕ web</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ docs.rs/tokio</span></pre>

</div>

MCP calls group by server under the `◈` marker:

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">6 tool calls</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">├─ </span><span class="bold">⬚ read</span>
  <span class="dim">│  ├─ crates/forge-tui/src/ui/message/grouping.rs</span>
  <span class="dim">│  └─ crates/forge-tui/src/ui/theme.rs</span>
  <span class="dim">├─ </span><span class="bold">◈ context7</span>
  <span class="dim">│  ├─ resolve-library-id</span>
  <span class="dim">│  └─ query-docs</span>
  <span class="dim">└─ </span><span class="bold">◈ playwright</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;├─ browser_navigate</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ browser_click</span></pre>

</div>

A single-kind run is a one-child tree - a lone read nests its file, a lone bash nests + clips its description:

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">1 tool call</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">└─ </span><span class="bold">⬚ read</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ crates/forge-tui/src/ui/message.rs</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span class="accent bold">⠋</span> <span class="bold">1 tool call</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">└─ </span><span class="bold">▶ bash</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ run the full workspace gate including fmt, clippy...</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">1 tool call</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">└─ </span><span class="bold">✦ skill</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ pr-review-loop 661</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">3 tool calls</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">└─ </span><span class="bold">➤ SendMessage</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;├─ to aa32ac1c4e464f26d: Add record-ordering check to roun...</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;├─ to planner: Resume. The account limit has lifted.</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ to steward: STOP AND CHECK YOUR SHA BEFORE GOING FURTHE...</span></pre>

</div>

A read path wider than the row middle-ellipsis keeps the filename visible; every other kind clips end-first on its own child row - the whole-summary truncation that used to drop trailing kinds is gone. Kinds render in first-appearance order across the run.

Clicking a group's summary row walks that group's level through L2 (summary) → L1 (per-tool title rows, bodies closed) → L0 (titles + full bodies). Single-item groups follow the same path as multi-item groups so the chat surface stays consistent regardless of run length. ctrl+x (Cmd+X) is a binary global toggle that clears every per-group override and resets to the global directive: collapsed → all groups at L2, expanded → all groups at L0. The intermediate L1Titles level stays reachable only via the per-group click cycle.

**L1 expansion** (titles only, bodies still closed):

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Read</span> crates/forge-tui/src/ui/message.rs
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Grep</span> crates/forge-tui/src "fn render"
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">WebSearch</span> ratatui Wrap trailing newline
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Bash</span> cargo nextest run -p forge-tui</pre>

</div>

**L0 expansion** falls through to the standard per-tool render: each row renders title + full body, indented under the standard `│  ` / `└─` tree connectors. Individual rows still honour their per-tool `collapsed_override`, so a click on a row at L1 can still flip just that row's body open.

**Mid-run breaker example**: a 3-Read run, an `Edit` breaker, then a 2-`Bash` run renders as two trees on either side of the `Edit` tool-card.

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">3 tool calls</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">└─ </span><span class="bold">⬚ read</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;├─ crates/forge-tui/src/ui/message.rs</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;├─ crates/forge-tui/src/ui/chat.rs</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ crates/forge-agent/src/env/git_diff.rs</span>
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Edit</span> crates/forge-tui/src/ui/message.rs
  <span class="dim">│  </span><span class="dim">+    let units = grouping::partition_blocks_into_render_units(&amp;msg.blocks);</span>
  <span class="dim">└─ </span><span class="dim">...</span>
  <span class="success">✓</span> <span class="bold">2 tool calls</span>   <span class="dim">ctrl+x to expand</span>
  <span class="dim">└─ </span><span class="bold">▶ bash</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;├─ cargo nextest run -p forge-tui</span>
  <span class="dim">&nbsp;&nbsp;&nbsp;└─ just check</span></pre>

</div>

- **code** - Render-class breaker predicate + partitioner: `crates/forge-tui/src/ui/message/grouping.rs` (`is_run_breaker` keys off `RenderToolCallContent::Diff` in `tc.content` and the by-name lifecycle / peer-block render sets; hidden / chat-suppressed tools pass through and don't break the run). Per-session level state: `UiSession.group_collapse_levels` (HashMap keyed by the leader tool's `tool_use_id`). Render dispatch: `append_assistant_blocks` in `ui/message.rs` partitions blocks per message and switches on level per group. Single-item groups behave identically to multi-item: a single-kind group renders a one-child tree, mouse-click on the summary row cycles L2 → L1 → L0 the same way. Per-kind tally (`KindSummary` / `KindLine`, glyph-family keyed with MCP-by-server; every kind keeps one target per call, uncapped) lives in `ui/message/grouping.rs`. L2 render: `ui/tool_call/group.rs::render_group_summary_line(summary, aggregate_status, spinner_frame, max_width, project_root, chrome)` - a parent count row + one `├─`/`└─` child per kind. Every kind nests one clipped child row per target regardless of call count (a target-less kind shows `×N` when called more than once); read relativizes each path against `project_root` (threaded from `cwd_raw` via `MessageRenderContext::with_project_root`) and clips with a middle-ellipsis (`clip_middle`) keeping the filename, every other kind clips end-first (`clip_to_width`). ctrl+x: `keys.rs::toggle_all_tool_calls` flips the session-wide `tools_collapsed` flag and invalidates `Global`.
- **cache** - The message render signature folds each Group's `(range, level, aggregate_status)` alongside the per-block hashes (`build_message_render_signature`), so a level flip or status transition on any one group invalidates only the affected message's cached layout. Spinner ticks invalidate the message cache (via the existing per-tool frame hash on InProgress tools) but NOT the layout cache - the single status_icon cell changes, line width stays stable.
- **focus model** - Mouse click on a group's summary row cycles that group's level (single-item groups follow the same path); the leader tool's `last_measured_y_in_msg` stamps the summary line's region so existing `locate_tool_call_block_at_click` resolves the click, and the mouse handler reclassifies via `grouping::group_hit_at`, scoping invalidation to `MessageChanged(msg_idx)`. At L2 the members are behind the summary and are NOT click targets: the render zeroes their hit-test rects, and a click that still resolves to one is refused rather than toggling a body that is not on screen. That refusal is reachable without any drift - the event loop drains queued terminal events without rendering between them, so a ctrl+x collapse and a click already in the tty buffer arrive back to back. ctrl+x stays bound to the session-wide global toggle - per-group state is independent of the global flag, so a click can cycle one group without disturbing the rest. Click on an individual row inside an L1 group flips just that row's body via the standard per-tool `collapsed_override` path.
- **invariant** - `tool_group_l2_summary_clears_hit_geometry_for_the_blocks_it_hides` (`message.rs`) pins that an L2 render leaves no member holding a clickable rect, and `click_resolving_to_a_tool_block_hidden_by_an_l2_summary_does_not_toggle_it` (`mouse.rs`) pins the refusal at the click. `every_special_render_tool_is_a_run_breaker` (unit test in `grouping.rs`) enumerates every diff / lifecycle / peer-block tool with a bespoke visible render path and asserts the breaker predicate returns true. Chat-suppressed tools (Task* / AskUserQuestion / Schedule* / Cron*) are covered by the inverse test `run_breaker_false_for_hidden_chat_suppressed_tools`: they render nothing visible and must pass through the run. Adding a new bespoke visible renderer requires extending both the predicate AND the test enumeration in the same change; otherwise the next group containing the new tool folds and the bespoke render never fires.

## Chat peer/worker messaging grouping (L2 / L1 / L0)

*visible: a run of 2+ consecutive peer/worker MCP messages (`peers__*` / `workers__*` outbound + inbound envelopes) **within one message**. A lone messaging block renders as its plain peer card.*

**Parallel to chat-tool-grouping**, grouped on the same scope: per message, with the message boundary a hard run-breaker (per #327). A run of peer/worker messages - outbound (`Tell`, `Ask`) and inbound (`Message`, `Question`, `Reply`) - folds into ONE messaging group; mixed direction lives in the same group (no split by direction). The partitioner is type-aware: peer/worker run produces a **messaging group**, tool-call run produces a **tool group**, adjacent and never merged - they sit side-by-side as separate render units.

**Consecutive incoming envelopes merge into ONE message at construction** (`sdk_message.rs::push_peer_envelope_user_turn_if_present`), so a run of incoming messages is one message with N blocks and reaches the threshold. The merge appends to the tail when that message is already an envelope of the same kind - gated on the constructor, because the role label is picked from per-message flags and a Gotify notification sharing a message with peer traffic would render an external alert as agent traffic. The window bounds itself: once the agent produces output the tail is no longer an envelope message, so the next envelope starts fresh.

**L2 summary is a TREE**, drawn by the same `render_group_summary_line` the tool groups use - a parent count row, one `├─`/`└─` child per kind, one leaf per message. It is not a new collapse level; it is what the summary line renders.

**The kind is the ENVELOPE KIND, not the direction.** A per-message group is always single-direction - inbound envelopes only live in user turns, outbound calls only in assistant turns - so a direction level would never discriminate. Envelope kind does: one turn genuinely fires two Tells and an Ask, and a run of incoming messages genuinely mixes a Reply with a Message. Seven kinds: `tell` and `ask` outbound, `message` / `question` / `reply` / `failed` / `spawn failed` inbound. Direction survives as the per-row glyph (`⤴` / `⤵`).

The parent row is a BARE count - no target list, because every peer appears as a leaf and a heading clause would name each one twice. Leaf rows are `<peer> · <first non-blank line of the body>`, clipped end-first to a computed per-row budget so nothing wraps and shears the tree. Every message keeps its own leaf: no `×N`, no `+N` overflow, because their bodies differ and collapsing loses the preview that makes the row worth having.

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">3 messages</span>   <span class="dim">click or ctrl+x to expand</span>
  <span class="dim">├─</span> <span class="bold">⤵ message</span>
  <span class="dim">│  ├─</span> <span class="dim">steward · IT IMPORTED. The window is lost…</span>
  <span class="dim">│  └─</span> <span class="dim">planner · picking up the migration now…</span>
  <span class="dim">└─</span> <span class="bold">⤵ reply</span>
  <span class="dim">   └─</span> <span class="dim">tester · take the render half, I'll take the partition…</span></pre>

</div>

**Always nests, never inlines.** A kind with one message still gets its own leaf, so peer names share a column instead of sitting at ragged widths. The **tool tree** nests on the same rule.

**Failures stay in the group and drive the parent status.** `failed` and `spawn failed` are kind rows like any other, styled as a warning. Pulling them out would break the run from inside the merged message and un-bundle everything either side of it. An inbound failure carries no `ToolCallStatus` of its own, so `aggregate_status` learns about it explicitly - without that the parent row shows a green check over a delivery that never arrived.

<div class="term">

  <pre class="indent">
  <span class="error">✗</span> <span class="bold">2 messages</span>   <span class="dim">click or ctrl+x to expand</span>
  <span class="dim">├─</span> <span class="bold">⤵ message</span>
  <span class="dim">│  └─</span> <span class="dim">steward · the window is lost</span>
  <span class="dim">└─</span> <span class="warning bold">⤵ failed</span>
  <span class="dim">   └─</span> <span class="dim">planner · gone</span></pre>

</div>

**A run does not continue past its message.** Consecutive incoming envelopes merge into one message, but the assistant's reply is still the next turn, so the two are separate messages and render as two separate cards - which is the intent: a message and the response to it are distinct events, not one bundle. A plain user turn between two runs breaks them apart for the same reason. Each group therefore covers exactly one message, and `KindSummary::total()` drives both the summary count and its pluralization.

<div class="term">

  <pre class="indent">
<span class="dim italic">// user turn - inbound envelope from steward</span>
  <span class="accent">&#x25B6;</span> <span class="bold">&#x2935;</span> <span class="bold">Message steward</span>
  <span class="dim">&#x2514;&#x2500;</span> the window is lost   <span class="dim">click or ctrl+x to expand</span>

<span class="dim italic">// assistant turn - the outbound reply, its own card</span>
  <span class="accent">&#x25B6;</span> <span class="bold">&#x2934;</span> <span class="bold">Tell steward</span>
  <span class="dim">&#x2514;&#x2500;</span> on it   <span class="dim">click or ctrl+x to expand</span></pre>

</div>

**L1 expansion** renders the per-message title rows (no body) using the existing five-verb peer-block contract; each row keeps its own direction kind-icon (`⤴` outbound, `⤵` inbound) and target label:

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">▶ ⤴</span> <span class="bold">Tell planner</span>
  <span class="success">✓</span> <span class="bold">▶ ⤴</span> <span class="bold">Ask debugger</span>
  <span class="success">✓</span> <span class="bold">▶ ⤵</span> <span class="bold">Reply from tester</span>
  <span class="success">✓</span> <span class="bold">▶ ⤴</span> <span class="bold">Tell lead</span>
  <span class="success">✓</span> <span class="bold">▶ ⤵</span> <span class="bold">Message from reviewer</span></pre>

</div>

**L0 expansion** falls through to the standard [peer-block render](./peers.md) per message - full body indented under `│  ` / `└─` tree connectors, each row's own `▶ Verb name` header naming the direction and the peer. Individual row body can still be re-collapsed via the standard `collapsed_override` path.

Click on a messaging group's summary row cycles its level (L2 → L1 → L0). `ctrl+x` (Cmd+X) is the binary global toggle: collapsed → L2, expanded → L0; the gesture also clears the per-group `messaging_group_collapse_levels` map alongside the tool-group map so a fresh resolve-from-global applies uniformly. `L1Titles` stays reachable only via the per-group click cycle.

- **code** - `partition_blocks_into_render_units` in `crates/forge-tui/src/ui/message/grouping.rs`: the tool-call pass runs first, then `merge_messaging_groups` folds runs of peer/worker blocks into `RenderUnit::MessagingGroup { segments, group_leader_id }`. Each segment carries `block_range`, `KindSummary::total()`, per-direction targets and `aggregate_status`. Tool-call run → `RenderUnit::Group`; adjacent peer + tool runs never merge. Collapse state lives in `UiSession.messaging_group_collapse_levels`, keyed on the group leader: the leading outbound block's `tool_use_id`, or for an inbound-led run the envelope's own correlation id (`inbound-<id>`). The id comes from the envelope rather than the block's position, so history pruning and index shifts cannot re-target a collapse level onto a different group.
- **cache** - The message render signature folds each `MessagingGroup` segment's `(block_range, level, aggregate_status, summary)` alongside the per-block hashes. A group lives entirely inside one message, so nothing another message does can change it and there is no cross-message cache coupling.
- **focus model** - Mouse click on a messaging-group summary row cycles the group's level via `locate_tool_call_block_at_click` (outbound leaders) or `locate_peer_user_block_at_click` (inbound leaders) plus `grouping::messaging_group_hit_at(messages, msg_idx, block_idx)`. That lookup partitions the same single message the renderer partitioned, so a hit-test and a group on screen cannot disagree about scope; the hit's `is_leader` distinguishes the leading block from members the summary hides, which are refused rather than toggled. The leading block of the group carries the summary line's region - `last_measured_y_in_msg` on a tool call, `peer_last_measured_y_in_msg` on an inbound text block. `ctrl+x` stays the global tools-collapsed toggle. The inbound half of this path is reachable now that consecutive envelopes merge into one message.
- **invariant** - In `grouping.rs`: `messaging_group_partitions_within_message_run`, `peer_run_across_turns_groups_per_message` (a run reaching the end of one message and continuing in a later one produces a separate group per message, with `assert_ne!` on the leaders so they cannot silently share a collapse key), `single_block_turns_do_not_group_across_the_boundary`, `messaging_group_splits_across_visible_block`, `messaging_and_tool_groups_never_merge`, and `inbound_led_runs_in_different_messages_get_distinct_leaders`, which pins that the collapse key comes from the envelope id rather than the block index. `peer_block.rs`'s `inbound_envelope_id_covers_every_header_shape` covers each real header plus the `None` rows that would otherwise fall back to a positional key. In `message.rs`: `peer_run_across_turns_renders_a_card_per_turn` pins the two-cards rendering above, and the three `*_clears_hit_geometry_*` tests pin that an L2 render leaves no hidden block holding a clickable rect. In `mouse.rs`: `click_on_messaging_group_summary_cycles_outbound_run` and its `_inbound_run` sibling cover both hit-test entry points, and `click_on_messaging_group_summary_invalidates_only_its_message` plus its inbound sibling assert the recorded invalidation level rather than a stale height - a stale height is what any invalidation produces, so only the level distinguishes a correct scope from a widened one. In `chat.rs`: `frame_cost_does_not_scale_with_session_size` fails if anything on the render path starts walking the whole session again, and `peer_dense_session_actually_forms_messaging_groups` guards its fixture against silently measuring nothing.

## Task* family (TaskCreate · TaskUpdate · TaskList · TaskGet)

*visible: never in chat - the Task\* family is silent in the scrollback. The live task state lives in the [Inspector pane](./inspector.md)'s `TASKS` section.*

CLI 2.1.156 retired the single-call `TodoWrite` in favour of an id-keyed quartet - `TaskCreate` (push one item, id parsed from result-text `Task #N created successfully:`), `TaskUpdate` (mutate by `taskId`; `status=deleted` removes), `TaskList` and `TaskGet` (read-only). Every call updates the Inspector pane in place; no chat noise. PR #269 shipped the renderer + reducer; PR #271's wire-conformance scenario locks the result-text parser shape.

## Workflow tool call (chat surface)

*not rendered in chat: the [Inspector pane](./inspector.md) `WORKFLOWS` section is the sole surface*

Workflow's `tool_input.script` carries a JS source blob with `export const meta = {name, description, phases: [{title, detail}]}`. `Workflow` is chat-suppressed: the tool call renders nothing in the chat stream, and the live phase tree in the [Inspector pane](./inspector.md)'s `WORKFLOWS` section is the only surface (same pattern as Task* → TASKS). The suppression is deliberate, not pending work: there is no Workflow chat block to keep honest.

- **code** - state accumulates on `UiSession.workflows`; suppression is the `"Workflow"` arm of `is_chat_suppressed` in `app/events/tool_calls.rs` - see Inspector WORKFLOWS section
- **icon** - kind icon `◆` (`U+25C6`, filled diamond) - distinct from Task/Agent `◇` (hollow diamond); RUST_ORANGE in the Inspector header

## Monitor tool call (chat surface)

*visible: live block in chat while the monitor runs; collapses to a one-line summary when it ends. No Inspector surface.*

Monitor's `tool_input` is `{description, command, persistent: bool, timeout_ms: u64}`. While the monitor is alive the chat block shows the header, the watched `$ command`, and the last 5 lines of the watched command's output (from the on-disk `output_file`, refreshed on each `system/task_progress` event), drawn with the same `│` / `└─` tree connectors the other live sections use. When the monitor stops / completes / times out the tail drops and the block becomes a single DIM summary line that stays in scrollback. The full output lives in the session transcript.

<div class="term">

  <pre class="indent">
  <span class="dim italic">// alive - last 5 lines, updating in place</span>
  <span class="accent bold">◉</span> <span class="bold">Monitor</span> <span class="dim">· ci-watch · persistent</span>
  <span class="dim">   │ $ gh run watch 18234567</span>
  <span class="dim">   │ * build  · in_progress</span>
  <span class="dim">   │ </span><span class="success">✓</span><span class="dim"> lint   · success</span>
  <span class="dim">   │ * deploy · queued</span>
  <span class="dim">   └ * notify · queued</span>

  <span class="dim italic">// done - collapsed, kept in scrollback</span>
  <span class="success">✓</span> <span class="dim">Monitor · ci-watch · completed</span></pre>

</div>

While alive the block is live: as the watched command emits output, `refresh_monitor_output_tail_from_file` stamps the last 5 lines onto the Monitor `ToolCallInfo` and bumps its `render_epoch`, so the cached chat block re-renders in place - the same mechanism backgrounded `Bash` uses to stream into chat. `TaskStop` renders as a standard tool_use card with the `◍` glyph; the resulting `task_updated` status flip collapses the block to its summary line.

**Which shape renders is decided by `ToolCallInfo.monitor_status`, mirrored from `MonitorEntry.status` - not by `ToolCallInfo.status`.** A Monitor's `tool_result` is only the `Monitor started (task ...)` ack and lands seconds after arming, so the TOOL CALL is `Completed` for nearly the whole time the monitor is alive. Reading it would collapse every running monitor - including a `persistent` one - to `✓ Monitor · <desc> · completed` the moment it started. The monitor's own liveness is a different fact and rides its own field: `Running` renders the live block, and each terminal variant renders its own word. In practice the wire produces only two of them - `handle_task_updated` maps `completed` to `Completed` and `failed` / `killed` / `stopped` all to `Stopped` - so `· timed out` has no live path today; the arm exists because `MonitorStatus` carries the variant.

**Every row clips to the render width, header included.** The `$ command` row and each tail row carry the tree connectors, and the outer layout char-wraps *without* the gutter, so an overflowing child row shears the tree. The header clips too, which is where this block DIVERGES from the peer and tool-group trees: those let their parent row wrap because its connectors live on the children, but a wrapped Monitor header lands flush-left *between* the header and the connector rows and breaks the tree from above. The header's budget cascades - the `◉` glyph column is fixed, then the `Monitor` label, then the description - and the `· persistent` suffix is dropped before it can push the row over. All of them clip through the same helper the other trees use.

- **code** - `crates/forge-tui/src/ui/message.rs::render_lifecycle_one_liner` renders the alive block (header + `$ command` + last-5 tail) and the collapsed summary; the tail is stamped onto `ToolCallInfo.monitor_output_tail` and the liveness onto `ToolCallInfo.monitor_status`, both from `app::state` keyed by the wire `task_id`. State still accumulates on `UiSession.monitors`. The Inspector MONITORS section is removed, so this block is Monitor's only surface anywhere in the TUI.
- **icon** - Monitor running: `◉` (`U+25C9`, fisheye) RUST_ORANGE bold; completed: `✓` green; stopped and timed out: `✗` red - the wire folds failed / killed / stopped all into `Stopped`, so a watched command that failed arrives as `· stopped` and must not read as a success. TaskStop: `◍` (`U+25CD`, circle with vertical fill - terminate). Tail lines + connectors: `DIM`.

## AskUserQuestion (multi-question dispatch)

*visible: every `AskUserQuestion` tool call (CLI 2.1.156+) - the model asks structured multi-option questions. UI is the existing [dock-morph widget](./input.md)*

AskUserQuestion's `tool_input.questions` is an array - a single tool call can carry N questions. forge dispatches each question onto the session's `prompt_queue` as a separate `PromptState`; the existing [dock-morph widget](./input.md) already supports the Q-of-N indicator (see the "Q2 of 3" mockup in the [Unified prompt](./input.md) section).

While the dock prompt is live the tool call is chat-suppressed (`hidden: true`). Once the user answers, the submit handler records the resolved answer onto the tool call (`App::record_answered_question`), which un-hides it and renders a compact **answered-card** in the scrollback, so the Q&A survives after the dock clears. A picked option shows its label; a typed "Other" answer shows the literal text the user entered. **multiSelect** may carry BOTH - picked options AND a typed Other note - in which case the card renders both on their own answer lines (picked labels first, then the typed line) so neither is lost. A multi-question call accumulates one question/answer pair per answered question.

Card chrome: the question line indents 2 spaces so the `?` lands in the tool-icon column (matching the standard tool row's `<icon>` at column 2); the answer line(s) nest one level deeper so the `→` sits at column 4, under the question text.

Wire shape per question: `{question, header, multiSelect, options: [{label, description}]}`. The `"(Recommended)"` suffix on `label` is a CLI-side convention - forge strips it, marks the option as recommended (bold), and pre-selects it. The universal "`... Tell Claude something else`" escape-hatch is appended to every question's options list (consistent with permission prompts).

<div class="term">

  <pre class="indent">
  <span class="accent bold">?</span> <span class="dim">How should the answered question appear in the chat?</span>
  <span class="dim">  → </span><span class="success">Clean answered-card</span>

  <span class="accent bold">?</span> <span class="dim">How should the answered question appear in the chat?</span>
  <span class="dim">  → you typed: </span><span class="bold">"Can you show me some visuals please?"</span>

  <span class="accent bold">?</span> <span class="dim">Which areas need work? (multiSelect)</span>
  <span class="dim">  → </span><span class="success">Performance, Documentation</span>
  <span class="dim">  → you typed: </span><span class="bold">"and the bot reviewer reply etiquette"</span></pre>

</div>

- **code** - `crates/forge-tui/src/app/events/tool_calls.rs` dispatches the AskUserQuestion tool_use to a sibling of `build_permission_options` (`build_question_options`) per question, pushing N `PromptState` entries onto `UiSession.prompt_queue`
- **icon** - answered-card: `?` in RUST_ORANGE (bold) on each question line, a DIM `→` before the answer (success-green for a picked label, bold for typed text); the dock-morph widget shows the same `?` on the live question header
- **render** - `crates/forge-tui/src/ui/message.rs::render_question_answered_card`, dispatched in `append_assistant_tool_block`; flagged a run-breaker in `grouping.rs::is_run_breaker` once un-hidden so the card never folds into a group

## Subagent (Task / Agent)

*chat-suppressed - the dispatch + every child tool call render in the [Inspector SUBAGENTS section](./inspector-processes.md), never in chat*

A Task / Agent dispatch is Inspector-only: the root tool call gets `ToolCallScope::SubagentRoot` and every nested child call gets `ToolCallScope::SubagentChild { parent_tool_use_id }`; both scopes set `hidden: true` in `tool_calls.rs:243` so the chat stream shows nothing for the entire subagent lifecycle. The SUBAGENTS Inspector section is the sole surface - it renders the root header (with subagent_type + first line of the prompt) plus a live tail of the last 3-4 child tool calls, auto-clearing once every root reaches a terminal status. See the [SUBAGENTS section](./inspector-processes.md) for the full chrome.

Backgrounded-task badges (`  [backgrounded]` / `  [assistant backgrounded]` via `tool_output_badge_spans`) remain available on the few non-subagent tools that still emit them (e.g. backgrounded Bash); subagent roots themselves never reach the chat tool-card renderer.
