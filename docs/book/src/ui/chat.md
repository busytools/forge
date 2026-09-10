# Chat - message types

Every message in the scrollback belongs to one role. Hovering shows an I-beam over selectable text, a hand over clickable blocks - the OS pointer via `OSC 22`, the default arrow at startup.

<details>
<summary>Pointer mechanics</summary>

The clickable set: tool calls, group headers, the scrollbar, pane rows. forge enables any-motion mouse tracking to drive the pointer shape, which the OS renders - forge does not paint it, so hovering never triggers a redraw. The default arrow is emitted once at startup, so the pointer is correct over chrome before the first hover.

</details>

## User message

The banner is the literal text "User" in dim bold; the body is a markdown block on a slate background.

<details>
<summary>User banner</summary>

The banner is not the user's name. The background is the theme's user-message color (see [Reference](./reference.md)) applied per cell, extending to roughly the right edge, with the text in the terminal's default foreground.

</details>

<div class="term">

  <pre class="indent">
  <span class="dim bold">User</span>
  <span class="user-band">  Read the rate-limit code and add a softer wording branch.                </span></pre>

</div>

## Assistant message

An assistant turn has no header row; a collapsible turn-info row trails the body - the spinner while the turn runs, `↳` once settled, dim `·`-separated fields, a `[▶ expand]` toggle. Body is full markdown.

<details>
<summary>Turn-info row: lifecycle and fields</summary>

The row appears when the turn starts and counts up: elapsed ticks, the thinking estimate and the input and cache tokens accumulate as each API call lands. Output tokens, cost and the API/local split do not exist until the turn's Result frame, so the collapsed row omits them and the expanded one dashes them rather than showing a zero.

A prompt submitted mid-turn with no cancel in flight joins the running turn rather than starting one (steering): the running row moves onto the fresh tail placeholder with its clock intact. A prompt submitted over a pending cancel restarts the row's clock, matching the interrupted turn's restart. A delivered turn (peer, worker, cron, gotify) stamps its clock at turn-open, so the row never renders as a bare loader.

**A running row may sit alone; a settled one may not.** Before any body exists the row is the only sign the turn is alive. Once the turn settles, a turn whose body rendered nothing visible gets no row, and a row with nothing stamped on it earns its line only while the session's turn clock is actually running.

**Once the row has figures, a compaction shows two spinners** - the compacting line says what is happening, the row says how long the turn has been going and what it has spent:

<div class="term">

  <pre class="indent">
  I'll fold the earlier context down before carrying on.

  <span class="accent">&#x280b; Compacting context...</span>
  <span class="dim">&#x280b; 4m 42s &#xb7; thinking 434 &#xb7; 45&#x2191; &#xb7; 95% cached &#xb7; 135k written [&#x25b6; expand]</span></pre>

</div>

A resumed session renders no turn-info row at all - replay synthesises only assistant and user messages, so neither half of the row is ever known.

Collapsed is the default and the expanded flag survives a re-render. While the turn runs the token field carries only its input half, and the elapsed ticks in whole seconds. The thinking estimate sits at position 2 and is a running-row field only:

<div class="term">

  <pre class="indent">
  <span class="dim">&#x280b; 12.0s &#xb7; thinking 434 &#xb7; 4.2k&#x2191; &#xb7; 93% cached &#xb7; 3.1k written [&#x25b6; expand]</span></pre>

</div>

Settled, the spinner becomes `↳`, the output half of the token pair arrives, and thinking drops out of the collapsed row - it stays visible in the expanded body:

<div class="term">

  <pre class="indent">
  <span class="dim">&#x21b3; 1m 19s &#xb7; 4.2k&#x2191; 1.1k&#x2193; &#xb7; 93% cached &#xb7; 3.1k written [&#x25b6; expand]</span></pre>

</div>

**Expanded** replaces the toggle label and adds an indented two-column body. An unknown cell renders `-`; a zero the wire actually reported is a measurement and renders as `0`:

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

**The body holds its height across the settle** - open it on a running turn and it does not grow a row under the cursor when the Result lands. `local`, `thinking` and `session` each hold their line as a bare dash: `local` without its `tools + hooks` tail, `session` without the `$` and the word `cumulative`:

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

**The cache-percentage line is the deliberate exception** - the one row that is a sentence rather than a labelled cell, appearing as soon as a cache read is known (the turn's first assistant frame).

</details>

<div class="term">

  <pre class="indent">
  Looking at <span class="dim">crates/forge-tui/src/app/events/rate_limit.rs</span> - the warning chip
  has only one wording today. I'll add a quieter branch for the
  near-threshold-no-overage case.

  Here's the function:

  <span class="code-panel">  <span class="code-label">rust</span>                                                                     </span>
  <span class="code-panel">  fn is_near_threshold_without_overage(                                    </span>
  <span class="code-panel">      update: &amp;model::RateLimitUpdate,                                     </span>
  <span class="code-panel">  ) -&gt; bool {                                                              </span>
  <span class="code-panel">      matches!(update.status, RateLimitStatus::AllowedWarning)             </span>
  <span class="code-panel">          &amp;&amp; update.is_using_overage == Some(false)                        </span>
  <span class="code-panel">          &amp;&amp; update.surpassed_threshold.is_some_and(|t| t &gt; 0.0)           </span>
  <span class="code-panel">  }                                                                        </span>

  Tests pass. Want me to push?
  <span class="dim">&#x21b3; 1m 19s &#xb7; 4.2k&#x2191; 1.1k&#x2193; &#xb7; 93% cached &#xb7; 3.1k written [&#x25b6; expand]</span></pre>

</div>

<details>
<summary>Turn-info rules: fields, durations, truncation, zeros, reuse</summary>

- Every span on the row and its expanded body is dim; the body above is the terminal's default foreground with markdown styles and a dim tint on inline code.
- Order: body, stop-hook summary chip, turn info, then the unconditional trailing separator - the row carries no leading blank, so messages stay one blank line apart.
- Durations: under 60 s one decimal (`12.4s`); from 60 s `1m 04s`; from 1h `1h 02m 04s`. Token counts render collapsed (`4.2k`, `1.4M`) in the collapsed row and full grouped (`1,102`) expanded.
- Order: body, stop-hook summary chip, turn info, then the unconditional trailing separator - the row carries no leading blank, so messages stay one blank line apart, and it carries no `turn info` label. The row sheds its first field at a terminal about twelve columns narrower than a labelled row would.
- Field scope: the wire mixes per-turn and session-cumulative fields, so the row cannot render a Result verbatim. `duration_api_ms` is session-cumulative - the turn's API time is its delta against the previous Result - and `local` is what is left after subtracting that delta, suppressed rather than clamped when the delta exceeds wall clock (concurrent subagent calls) or when the counter resets after a compaction. `total_cost_usd` is session-cumulative, hence the `session` label. `num_turns` counts agentic iterations within one request, not the session's turns, and is not rendered.
- Missing values: an absence is never rendered as `0` - a zero is a claim and an absence is not. An absent count drops out of the collapsed row and renders `-` in the expanded one; `local`, `thinking` and `session` hold their line with a dash. `local` and `session` fill only when the Result lands; `thinking` fills during the turn if it fires at all and stays a dash for a turn that never thought. The cache percentage is the single row that still drops rather than dash.
- Zero is not a measurement: a `duration_api_ms` of zero means the CLI attributed no API time (the counter is millisecond-granular - a turn that reached the API cannot register zero), and an all-zero usage block says the same about tokens. Neither reaches the row - `api` and `local` both render `-` while the token cells keep the turn's own running counts. The rule keys on the whole usage block: a lone zero inside a real one is a measurement and renders as `0`.
- A row never mixes two Results: a compaction can emit a Result with no assistant message at all. A Result carrying usage overwrites every accounting field together and is allowed through even onto a settled row; one with no usable token counts is refused by a settled row. A compaction whose Result arrives after the previous turn settled renders no row - the one shape where a turn has no row of its own; one reaching a still-live row does stamp its clock there.
- A turn reusing an unsettled row - a live tail whose pointer was lost, or a wire user prompt, which opens no placeholder of its own - shares the row until its Result replaces the fields, and both live writers skip a settled row, so reuse never lands on finished figures. A tool call opening after a settled row starts its own message rather than gluing in; a Result racing a mid-turn submit settles on the body row the submit shed its bar from.
- The end time is stamped locally on arrival - wall-clock is not on the wire. Running fields come from the assistant envelope's usage, deduplicated on the message id because the CLI splits one assistant message across a frame per content block and repeats the usage on each; the output tokens on those frames is the streaming placeholder, not a count, until the Result lands.
- A Monitor sitting between two peer envelopes drops that pair below the messaging-group threshold, so the pair renders as plain peer cards.
- Cache percentage: cache reads over inputs plus cache writes - both cache counters are input tokens (one written at a premium, one read back cheaply), so there is no output cache and neither is ever labelled as output. The expanded row names the denominator inline.
- Thinking estimate: summed from per-thinking-block deltas - the event's counters restart per block inside a single turn (measured across the 2.1.220 baselines: `exit_plan_mode` runs 50, 164 then restarts at 50, 150, 250, 270; `permission_request_hook` the same at 50, 71 then 50, 150), so summing every delta is the exact per-turn total with no boundary to detect - verified equal to the sum of the per-block finals on all nine baselines that carry the event, with no negative delta anywhere. It is labelled `est` because it is the CLI's estimate of reasoning tokens, is not billed, and sits in the body rather than beside the billed counts. A turn that fired no such event renders `-`.
- Settled fields stamp from the turn's Result; when the Result finds the tail placeholder a mid-turn submit opened still empty, the stamp diverts to the nearest earlier unsettled body row. `system/turn_duration` never fires on the 2.1.156 wire (verified across 43 baselines and 14 fresh captures, all zero) - the prior chip read from that dead event and was deleted in #283.

- Turn start resets the row and the accumulator together, and the usage stamp and settle assign unconditionally, so a turn reusing an unsettled row never inherits the previous turn's estimate.
- Ticking: no new timer - the row's cache key carries elapsed whole seconds, so elapsed moves the key only on a second boundary; the spinner glyph folds into the same signature and turns over every 32 ms on the default braille style, so a live turn's message re-lays out around 31 times a second. Measured at roughly 28µs collapsed against 148µs expanded for a one-paragraph turn, an open body costs about 120µs per rebuild - 3.75 ms of work per second, 22.5% of one 60 fps frame budget.

</details>

<details>
<summary>Expanding the row</summary>

Click the row to toggle it - the same affordance and hand pointer as the stop-hook summary chip. **Cmd+X** (Ctrl+X off macOS) toggle-all clears every per-row override in the active session, so anything clicked open or shut returns to what the flipped flag dictates; the symmetry is one-way - expand-all opens the tool calls and still shuts the row, which has no global state of its own.

</details>

## Code block

A fenced code block is a quiet panel: a lifted background, no box-drawing glyphs and no fence delimiters, on both roles. The fence's info string is a dim label on the panel's first row; the code under it is syntax highlighted through the same lookup tool-call bodies use. Backtick and tilde fences both work, a closing fence must be at least as long as its opener, and either fence line may be indented up to three spaces.

<div class="term">

  <pre class="indent">
  <span class="code-panel">  <span class="code-label">toml</span>                                                                     </span>
  <span class="code-panel">  [accounts.env]                                                           </span>
  <span class="code-panel">  CLAUDE_CODE_OAUTH_TOKEN = "..."                                          </span></pre>

</div>

<details>
<summary>Code block rules</summary>

- The panel owns its wrapping. A long line wraps inside the panel instead of running past its right edge, and every row carries the background to the panel's full width, so the block reads as one surface rather than a patch per span.
- The info string labels the panel whenever the fence carries one, and goes to the syntax lookup whole and trimmed. A language syntect cannot resolve still labels the block; its code renders plain.
- An unterminated fence stays a panel to the end of the message, so a code block still streaming never flickers between panel and prose.
- Blank prose around the fence collapses to a single separator row above the panel.
- The panel is a plain body block: no collapse state and no click target.

</details>

## Compacting indicator

While the session compacts the active assistant's status slot shows the spinner frame and "Compacting context..." in rust orange.

<details>
<summary>Compacting details</summary>

The line trails the body after a blank line (the whole body on a body-less placeholder), is the slot's only non-dim line, arms wire-driven, never optimistically, and clears when the CLI reports the settle. Arming rides the session-status stream's `compacting` status, each typed `compact_boundary` re-arms it (recording the trigger and pre-tokens), and it clears on the settle - the same moment a manual `/compact` emits its success notice. It does not replace a turn-info row that already has figures; the two render together (the two-spinner case above).

</details>

<div class="term">

  <pre class="indent">
  <span class="accent">&#x280b; Compacting context...</span></pre>

</div>

## Stop-hook summary

When Stop-event hooks ran: a collapsed one-liner - click `[▶ expand]` (or the keyboard toggle) to expand. Hidden at zero actions. All dim (CLI 2.1.156+).

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

## System notice

Rate-limit warnings, mode changes, parse errors, retries, slash-command output, connection failures. Banner: the literal severity word in bold - "Info" dim, "Warning" warning-yellow, "Error" error-red - body lines tinted to match, hard-wrapped.

<details>
<summary>Notice placement</summary>

Mid-turn the notice inserts just above the active assistant placeholder (the spinner stays put and the response keeps streaming there); idle or at turn end it appends at the tail. A rate-limit warning reaches the same inline position.

</details>

<div class="term">

  <pre class="indent">
  <span class="warning bold">Warning</span>
  <span class="warning">Near rate-limit threshold. Resets in 4h 23m at 14:30 UTC.</span>

  <span class="error bold">Error</span>
  <span class="error">Failed to parse ~/.claude/settings.json: expected `,` at line 42 column 5. Falling back to defaults.</span>

  <span class="dim bold">Info</span>
  <span class="dim">Mode changed: default → acceptEdits</span></pre>

</div>

# Tool calls

Every tool invocation renders through one path: a title row at column 2, body lines prefixed with a dim `  │  `, last `  └─ `. All tools share one shape; the body varies by kind.

## Standard row

Single line: 2-space indent, status icon in its status color, kind icon and kind label in white bold, then the display title in default foreground. Body lines prefix with a dim `  │  `, last `  └─ `.

<details>
<summary>Collapse behavior</summary>

**Collapsed** (when collapse is in effect and the call carries no diff, pending permission or pending question): the body folds to one dim summary line, `  └─ <summary>  click or ctrl+x to expand` - the summary is the last non-empty output line (capped at 80 chars), or the file name for a diff, or an MCP resource path.

**Carved-out kinds render expanded regardless of the global collapse**: Bash, diff content (Edit / Write / MultiEdit / NotebookEdit), and any tool that renders as a lifecycle block - keyed on the render, so a Monitor whose input does not parse paints an ordinary card and stays collapsible.

**Ctrl+X (Cmd+X) is a binary global toggle**, not a graduated cycle: once expands every non-carved-out kind, again collapses everything, and it clears every per-item override so a row clicked open or shut resets to its default-render state. The intermediate titles-only group level stays reachable only by clicking a group's summary row. Session start defaults to collapsed, so a fresh chat opens compact.

</details>

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

<details>
<summary>Server-side tool variants</summary>

Server-side tool variants (ToolSearch, web_search, web_fetch, advisor, plus the code-execution family) render through the same standard path - the lowercase wire names map back to the familiar capitalised labels, so the card chrome is identical to the in-process equivalents. ToolSearch also arrives as a client `tool_use` carrying `{query, max_results}`; both shapes surface the query as `⌖ ToolSearch <query>`, and its results render as a compact `Found <A>, <B>` body line rather than a raw schema dump.

</details>

<details>
<summary>Title mangling</summary>

**Plan-mode aliases**: in plan mode the Write title becomes "Create Plan" and Edit / MultiEdit "Update Plan" - the kind label still renders. A title claude already sends starting with the kind label is not doubled.

</details>

| Kind | Glyph |
|---|---|
| Read | `⬚` |
| Write / Edit / MultiEdit / NotebookEdit / Delete | `▣` |
| Grep / Glob / LS | `⌕` |
| Bash | `▶` |
| WebFetch / WebSearch | `⊕` |
| Move / EnterWorktree | `⇄` |
| ExitPlanMode / Config | `⊙` |
| TodoWrite | `◌` |
| Task / Agent (labelled "Subagent") | `◇` |
| ToolSearch | `⌖` |
| Skill / Advisor | `✦` |
| Fallback (labelled "Tool") | `○` |

Status icons: `✓` success, `✗` failure, the spinner while running.

## Bash row

The standard shape with the command as title; the body carries the terminal output capped at the last 12 lines; on failure only the first stderr line shows.

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Bash</span> cargo nextest run -p forge-tui events::rate_limit
  <span class="dim">│  </span>$ cargo nextest run -p forge-tui events::rate_limit
  <span class="dim">│  </span>   <span class="dim">Compiling forge-primitives v1.0.53</span>
  <span class="dim">│  </span>   <span class="dim">Compiling forge-tui v1.0.53</span>
  <span class="dim">└─ </span><span class="success">Summary [4.197s] 5 tests run: 5 passed, 1174 skipped</span></pre>

</div>

## Tool-call grouping (L2 / L1 / L0)

Consecutive tool calls between two user messages fold into one foldable group (threshold 1). Run-breakers (the diff tools, Monitor, the peer-block tools) render inline and split the run; chat-suppressed tools pass through so visible groups merge.

<details>
<summary>Group shapes, levels, click behavior</summary>

The L2 summary is one tree for every run: a parent count row - the aggregated status icon (spinner while any call runs, green check when all complete, red cross if any failed, hollow circle while only pending), bold glyph and label, the dim `ctrl+x to expand` hint - then one dim-connected child per kind (`├─` / `└─` with a `│` spine), each kind nesting one child row per resolved target (uncapped), or a bare row with `×N` when a target-less kind repeats. Same-glyph tools merge: Grep / Glob / LS become one `search` child, WebFetch / WebSearch one `web`, LSP `lsp`; MCP calls group by server under the `◈` marker, one child per call.

Click a group's summary row to cycle it L2 (summary) → L1 (title rows) → L0 (title + full body, the standard per-tool render); a click on a single row inside an L1 group flips just that row's body. At L2 the members are hidden behind the summary and are not click targets - a click resolving to one is refused rather than toggling a body that is not on screen. Ctrl+x stays bound to the session-wide global toggle; per-group state is independent of it. Each group's range, level and aggregate status fold into the message's render signature, so a level flip invalidates only that message's cached layout; spinner ticks invalidate the message cache but not the layout - the status cell changes while the width stays stable.

</details>

<details>
<summary>Row content and clipping</summary>

- Nothing wraps: each row is a single line, and the nested target rows are the only ones that clip. Read relativizes each path against the project root and clips with a middle-ellipsis so the filename stays visible; every other kind clips end-first with `...`, keeping the head. The parent count row is never clipped and often the widest; the target budget floors at 8 cells, so below a render width of 16 a child row overflows, and the outer layout char-wraps without the tree gutter, so an overflowing row shears the tree.
- Per-kind content: bash shows the human-readable description, web the URL (scheme stripped) or query, toolsearch the query, skill the invoked skill name (plus its args), SendMessage the recipient and summary (falling back to the full message), Delete / Move their paths, LSP the operation and file, PushNotification the message.
- Kinds render in first-appearance order; the spine holds `│` while a later kind follows, blank on the last.

</details>

Multi-kind run:

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

MCP by server:

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

A single-kind run is a one-child tree:

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

L1 expansion (titles only, bodies still closed):

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Read</span> crates/forge-tui/src/ui/message.rs
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Grep</span> crates/forge-tui/src "fn render"
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">WebSearch</span> ratatui Wrap trailing newline
  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Bash</span> cargo nextest run -p forge-tui</pre>

</div>

A mid-run breaker:

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

## Messaging grouping (L2 / L1 / L0)

A run of 2+ consecutive peer/worker messages within one message folds into one messaging group; a lone one renders as a plain peer card.

<details>
<summary>Messaging group shape and kinds</summary>

The L2 summary is the same tree the tool groups draw: a bare count parent (no target list), one child per envelope kind, one leaf per message - `<peer> · <first non-blank line>`, clipped so nothing wraps. Collapse levels key on the group leader's correlation id (an inbound run keys on the envelope's own `inbound-<id>`) rather than the block's position, so history pruning and index shifts cannot re-target a level onto a different group. The kind is the envelope kind, not the direction: `tell` / `ask` outbound, `message` / `question` / `reply` / `failed` / `spawn failed` inbound; direction survives as the per-row glyph (`⤴` out, `⤵` in). A kind with one message still gets its own leaf, every message keeps its own leaf (no `×N`), and failures stay in the group as warning-styled kind rows that drive the parent status icon. Consecutive incoming envelopes merge into one message at construction so an inbound run reaches the threshold; a Gotify notification sharing the message breaks the merge, so an external alert never renders as agent traffic. Mixed directions live in the same group.

</details>

<details>
<summary>Run boundaries and levels</summary>

A run never continues past its message: the assistant's reply is the next turn and renders as its own card; a plain user turn between two runs breaks them apart. Click a group's summary row to cycle L2 → L1 (per-message title rows, each with its direction glyph and target label) → L0 (the standard [peer-block render](./peers.md), each row re-collapsible); ctrl+x is the same global binary toggle.

</details>

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">3 messages</span>   <span class="dim">click or ctrl+x to expand</span>
  <span class="dim">├─</span> <span class="bold">⤵ message</span>
  <span class="dim">│  ├─</span> <span class="dim">steward · IT IMPORTED. The window is lost…</span>
  <span class="dim">│  └─</span> <span class="dim">planner · picking up the migration now…</span>
  <span class="dim">└─</span> <span class="bold">⤵ reply</span>
  <span class="dim">   └─</span> <span class="dim">tester · take the render half, I'll take the partition…</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span class="error">✗</span> <span class="bold">2 messages</span>   <span class="dim">click or ctrl+x to expand</span>
  <span class="dim">├─</span> <span class="bold">⤵ message</span>
  <span class="dim">│  └─</span> <span class="dim">steward · the window is lost</span>
  <span class="dim">└─</span> <span class="warning bold">⤵ failed</span>
  <span class="dim">   └─</span> <span class="dim">planner · gone</span></pre>

</div>

A plain user turn between two runs breaks them apart:

<div class="term">

  <pre class="indent">
<span class="dim italic">// user turn - inbound envelope from steward</span>
  <span class="accent">&#x25B6;</span> <span class="bold">&#x2935;</span> <span class="bold">Message steward</span>
  <span class="dim">&#x2514;&#x2500;</span> the window is lost   <span class="dim">click or ctrl+x to expand</span>

<span class="dim italic">// assistant turn - the outbound reply, its own card</span>
  <span class="accent">&#x25B6;</span> <span class="bold">&#x2934;</span> <span class="bold">Tell steward</span>
  <span class="dim">&#x2514;&#x2500;</span> on it   <span class="dim">click or ctrl+x to expand</span></pre>

</div>

L1 expansion:

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">▶ ⤴</span> <span class="bold">Tell planner</span>
  <span class="success">✓</span> <span class="bold">▶ ⤴</span> <span class="bold">Ask debugger</span>
  <span class="success">✓</span> <span class="bold">▶ ⤵</span> <span class="bold">Reply from tester</span>
  <span class="success">✓</span> <span class="bold">▶ ⤴</span> <span class="bold">Tell lead</span>
  <span class="success">✓</span> <span class="bold">▶ ⤵</span> <span class="bold">Message from reviewer</span></pre>

</div>

## Task* and Workflow

<details>
<summary>Task* semantics</summary>

CLI 2.1.156 retired the single-call `TodoWrite` in favour of the id-keyed quartet: TaskCreate pushes one item with its id parsed from the result text (`Task #N created successfully:`), TaskUpdate mutates by id (`status=deleted` removes), TaskList and TaskGet read. Every call updates the Inspector pane in place - no chat noise.

</details>

The Task* quartet and `Workflow` render nothing in chat - live state lives in the [Inspector](./inspector.md)'s `TASKS` and `WORKFLOWS` sections (Workflow's icon: `◆`).

## Monitor

While the monitor runs the block shows the header, the `$ command`, and the last 12 output lines updating in place; when it ends a dim summary line stays. Monitor's only surface in the TUI.

<div class="term">

  <pre class="indent">
  <span class="dim italic">// alive - last 12 lines, updating in place</span>
  <span class="accent bold">◉</span> <span class="bold">Monitor</span> <span class="dim">· ci-watch · persistent</span>
  <span class="dim">   │ $ gh run watch 18234567</span>
  <span class="dim">   │ * build  · in_progress</span>
  <span class="dim">   │ </span><span class="success">✓</span><span class="dim"> lint   · success</span>
  <span class="dim">   │ * deploy · queued</span>
  <span class="dim">   └ * notify · queued</span>

  <span class="dim italic">// done - collapsed, kept in scrollback</span>
  <span class="success">✓</span> <span class="dim">Monitor · ci-watch · completed</span></pre>

</div>

<details>
<summary>Monitor status and TaskStop</summary>

`◍` TaskStop renders as a standard card; its status flip collapses the block. Which shape renders is keyed on the monitor's own status, not the tool call's - the tool call completes seconds after arming while the monitor lives on. The tail restamps from the watched command's on-disk output file on each `system/task_progress` event, so the cached block re-renders in place as output arrives. Running renders the live block; the wire folds failed / killed / stopped all into `Stopped`, so a watched command that failed arrives as `· stopped` and must not read as a success. `· timed out` has no live path today; the arm exists because the status carries the variant. The full output always lives in the session transcript.

</details>

Running renders `◉` in rust orange bold, completed `✓` green, stopped `✗` red; TaskStop is `◍`.

<details>
<summary>Header clipping</summary>

Every row clips to the render width, header included: the header's budget cascades - the `◉` glyph column, then the `Monitor` label, then the description - and the `· persistent` suffix is dropped before it can push the row over. The peer and tool-group trees let their parent row wrap because its connectors live on the children; this header clips.

</details>

## AskUserQuestion

One call can carry N questions (CLI 2.1.156+); each dispatches as its own dock prompt - the Q-of-N widget in [Input](./input.md).

<details>
<summary>Answered cards</summary>

While the dock prompt is live the tool call is chat-suppressed; once answered it un-hides and renders a compact answered-card so the Q&A survives after the dock clears. A picked option shows its label; a typed "Other" answer shows the literal text. A multiSelect answer may carry both - picked labels first, then the typed line - and a multi-question call accumulates one pair per answered question. A `(Recommended)` suffix on a label is stripped and the option pre-selected; the "... Tell Claude something else" escape hatch appends to every question's options.

</details>

<details>
<summary>Answered-card chrome</summary>

The question line indents 2 spaces so the rust-orange bold `?` lands in the tool-icon column; the answer lines nest one level deeper, a dim `→` before the answer (green for a picked label, bold for typed text). Once un-hidden the card is a run-breaker, so it never folds into a group.

</details>

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

## Subagent

Chat-suppressed: the dispatch and every child call render only in the [Inspector SUBAGENTS section](./inspector-processes.md). Backgrounded-task badges remain available on non-subagent tools.
