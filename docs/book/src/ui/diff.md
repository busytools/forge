# Diff viewer

## Inline diff (Edit / MultiEdit / Write)

Every Edit / MultiEdit / Write tool call that carries a diff renders inline in the tool body: an optional `[repository]` tag in dim, a cyan compacted `@@` hunk header, and change lines with a `-` red / `+` green marker plus a left-padded line number (width sized to the larger of the old and new counts).

<div class="term">

  <pre class="indent">
  <span class="success">✓</span> <span class="bold">▣</span> crates/forge-tui/src/app/events/rate_limit.rs (+34, -2)
  <span class="dim">│    </span><span class="dim">[forge]</span>
  <span class="dim">│    </span><span style="color: cyan;">@@ -65,4 +65,12 @@</span>
  <span class="dim">│    </span> 65 fn is_org_level_disabled_extra_usage_case(...)
  <span class="dim">│    </span><span class="success">+ 66 fn is_near_threshold_without_overage(</span>
  <span class="dim">│    </span><span class="success">+ 67     update: &amp;model::RateLimitUpdate,</span>
  <span class="dim">│    </span><span class="success">+ 68 ) -&gt; bool {</span>
  <span class="dim">│    </span><span class="success">+ 69     matches!(update.status, RateLimitStatus::AllowedWarning)</span>
  <span class="dim">│    </span><span class="success">+ 70         &amp;&amp; update.is_using_overage == Some(false)</span>
  <span class="dim">│    </span><span class="success">+ 71         &amp;&amp; update.surpassed_threshold.is_some_and(|t| t &gt; 0.0)</span>
  <span class="dim">│    </span><span class="success">+ 72 }</span>
  <span class="dim">│    </span> 73 pub(super) fn format_rate_limit_summary(...)
  <span class="dim">└─   </span><span class="error">- 74 // old comment that's no longer accurate</span></pre>

</div>

<details>
<summary>Inline diff details</summary>

The body sits inside the standard tool-call indent: the dim body prefix (5 cells) plus 2 more cells, so diff content starts at column 7. Hard tabs in the source expand to spaces at 4-column stops measured from the start of the source line - the same expansion the raw unified-diff path and the `/diff` overlay use, so a tab-indented file (Go) renders at the same depth as a space-indented one (Rust). Every other control character swaps for its Control Pictures glyph (form feed, ESC, DEL, and C1 as the replacement character) at the single point both bodies read cached spans from: a raw one measures a column and paints none, so a split row under-fills and its `│` lands left of the divider the click handler splits sides on. Bash output that does not read as a unified diff renders as plain text and keeps raw tabs. Write-tool diffs cap their length (head + ellipsis + tail). Markdown files render through the same path as code files - no special case.

</details>

## Full-screen diff overlay (`/diff`)

`/diff` or `/diff <target>`, or a click on the Inspector `🦉`, opens a full-frame review viewer: chat, input and both panes disappear while it is up. One continuous scroll of every changed file in the FILES rail's folded directory-tree order - one sequence shared by the rail, the body, and the current-file marker, so scrolling down steps the marker monotonically down the rail. The default `/diff` reads the Inspector GIT snapshot: a dirty worktree reviews `git diff HEAD`; a clean feature branch reviews the diff against the default branch. `/diff <target>` reviews the named ref against the working tree.

| Key | Action |
|---|---|
| <kbd>↑↓</kbd> / <kbd>PgUp/Dn</kbd> / wheel | Scroll the document (one scroll flows from the first file to the last) |
| Click a file in the FILES rail | Jump to that file |
| <kbd>t</kbd> | Flip the document between unified (default) and split |
| Click a diff line | Attach a comment |
| Click a `┈ ↕ N lines ┈` expander | Reveal hidden context |
| <kbd>l</kbd> | Open the REVIEWS list of past review passes |
| <kbd>Esc</kbd> | Finish the review (see below) or close |

<details>
<summary>Two surfaces, two modes</summary>

A FILES jump rail sits on the left (15% of width, minimum 20ch; hidden below 120 cols, where the body goes full-width), and the continuous diff body on the right. When the target has commits ahead the overlay opens in **commit mode** - a stepper walks the commits oldest to newest, each scoped to just that commit's diff, with per-commit comment scoping; with no commits ahead it opens in **whole-diff mode**. "All changes" in the jump dropdown returns to the whole-branch view.

</details>

<details>
<summary>FILES rail</summary>

Banner `FILES` + dim rule, then a box-drawing tree mirroring the [Inspector GIT section](./inspector.md)'s shape - single-child directory chains collapse into one row; directory rows carry no marker. File leaves: the connector, a rust-orange `▸` on the file whose top sits at the viewport top (the same file the sticky header pins - the marker steps down the rail as the body scrolls), a status glyph (`M`/`A`/`D`/`R`/`C`/`T`/`U`, `!` for Unmerged), the filename, and an orange `💬 N` badge when N saved comments anchor in that file. Clicking a file jumps the document to its first row (closing any open comment editor); clicking a directory does nothing; the wheel scrolls the rail. Untracked files appear with the `U` glyph, capped at 4 per scan (each up to 1 KB); past the cap a yellow `+N untracked suppressed (cap M)` line follows the tree.

</details>

<details>
<summary>Diff body</summary>

Each file opens with a banded sticky header - a filled bar spanning the pane width: caret (`▾` expanded / `▸` collapsed), bold path, status badge, and right-justified `+N -M` totals. The header of the file at the viewport top stays pinned while its body scrolls beneath it. Every file but the last closes with a dim `└─ end <path> ───` cap row and a spacer, so the next file's band pins as the old cap scrolls up. Where lines are hidden - above a file's first hunk or in an inter-hunk gap - a dim clickable expander (`┈ ↑ N lines ┈` / `┈ ↕ N lines ┈`) shows the count; clicking widens that file's context about 20 lines per side, revealed in memory from a wide-context snapshot captured once at open (nothing is fetched on click), the row shrinking and vanishing as the gap closes. The snapshot is bounded, so a big file with a small change does not emit its whole content; a file whose snapshot would still blow the 8 MiB scan cap renders a bounded diff with a dim `(file too large - context expansion disabled)` note and no expanders, without blanking the rest of the scope.

**Unified** (default): one column - line-number gutter, sign, syntax-highlighted text - removed lines then added within a hunk, `+` on a dark-green tint, `-` on a dark-red tint, context plain. Long lines soft-wrap: continuation rows blank the gutter and sign and align under the text column (no horizontal scrolling). **Split** (after <kbd>t</kbd>): the side-by-side view - old file (context + removed) left, new file (context + added) right, dim `│` divider, each half truncating long lines; one function computes the divider column, so the painted column and the click boundary cannot drift; below 100 cols of body width the split toggle silently falls back to unified, and the stored choice returns when the pane widens. Hunk headers render cyan. In-text colors come from syntect for Rust / TS / JS / Python / Go / JSON / TOML / YAML / Markdown / Shell, with two stateful passes per file so multi-line constructs (block comments, strings) keep their state within each side; spans are computed once when a file first enters the viewport and cached, so plain scroll, the split toggle and resizes never re-run them. **Deleted files** collapse by default to a one-line `File deleted - N lines removed` notice; click the header (or the notice) to expand the full body.

</details>

<details>
<summary>Commit mode</summary>

Two rows pin above the FILES rail: a title (`COMMITS · <branch> vs <target> · N commits`) and a controls row (`◀ [i / N] <sha> <subject> ▶`, the `⌄ jump` affordance, and a running `● N comments so far` total of the branch's comments, counted once however many scopes draw them - in commit mode this total replaces the whole-diff footer's `N comments pending` prefix). <kbd>◀</kbd>/<kbd>▶</kbd> or <kbd>[</kbd>/<kbd>]</kbd> step commits (clamped); each commit's hunks are scanned lazily on first visit and cached, with a brief `Loading commit diff...` while a scan runs. Comments accumulate across commits, so the final <kbd>Esc</kbd> submits them grouped by commit. The current commit's full message - bold subject, dim soft-wrapped body behind a rust-orange rail - leads the diff body and scrolls with it. <kbd>a</kbd> toggles between the current commit and the whole-branch view, remembering the commit. The whole-diff view is a union and a commit's view is a filter over the same store: every comment on the branch renders in "All changes" whichever commit it was left against, while a commit shows only what was authored on it - so a rebase that rewrites the commit a comment was made against re-anchors it against the whole-branch diff rather than losing it (a comment anchored against a different diff base is the one thing the union excludes). The <kbd>j</kbd> jump dropdown is a GitHub-style commit picker: `All changes` at the top, then the commits with per-commit `● N` comment counts and the current commit marked `◂`; <kbd>↑↓</kbd> move, <kbd>Enter</kbd> jumps, <kbd>Esc</kbd> or a click-away closes the menu, leaving the overlay open.

</details>

<div class="term">

  <pre class="indent">
<span class="dim">┌─ Diff review ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐</span>
<span class="dim">│</span>  <span class="accent-bold">  FILES · 4</span>                   <span class="dim">│</span><span style="background:#1b2130">  <span class="dim">▾</span> <span class="bold">app/diff_overlay.rs</span>   <span class="accent">modified</span>                                                        <span class="success">+42</span> <span class="error">-18</span> </span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────</span>  <span class="dim">│</span><span class="dim">┈┈┈ ↑ 12 lines ┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈</span><span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>   <span style="color: cyan;">@@ -470,7 +470,9 @@</span>                                                                            <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">crates/forge-tui/</span>             <span class="dim">│</span>     <span class="dim">470</span>     pub current_file_idx: usize,                                                         <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">├─</span> <span class="accent">▸</span> <span class="accent">M</span> app/diff_overlay.rs <span class="accent">💬1</span><span class="dim">│</span>     <span class="dim">471</span> <span style="background:#67060c">&nbsp;<span class="error">-</span> pub body_scroll: u16,&nbsp;</span>                                                                <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">├─</span>   <span class="accent">M</span> ui/diff_overlay.rs    <span class="dim"> │</span>         <span style="background:#033a16">&nbsp;<span class="success">+</span> /// Scroll across the whole diff doc.&nbsp;</span>                                                <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">└─</span>   <span class="success">A</span> env/git_diff/hunks.rs <span class="dim"> │</span>    <span class="dim">472</span> <span style="background:#033a16">&nbsp;<span class="success">+</span> pub doc_scroll: u32,&nbsp;</span>                                                                  <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>     <span class="dim">473</span>     pub comments: Vec&lt;HunkComment&gt;,                                                      <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">╭─ </span><span class="bold">💬 line 471</span> <span class="dim">· unfiled ···········</span> <span class="accent">OPEN</span> <span class="dim">─╮</span>                                             <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent">●</span> <span class="accent">you</span>                                   <span class="dim">│</span>                                             <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">│</span>    doc_scroll is the only scroll now     <span class="dim">│</span>                                             <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent-bold">✓ Resolve</span>   <span class="dim">↺ Reopen</span>                    <span class="dim">│</span>                                             <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">╰──────────────────────────────────────────╯</span>                                             <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span><span class="dim">└─ end app/diff_overlay.rs ───────────────────────────────────────────────────────────────────────</span><span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>                                                                                                  <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span><span style="background:#1b2130">  <span class="dim">▸</span> <span class="bold">ui/diff/old_split.rs</span>   <span class="error">deleted</span>                                                        <span class="success">+0</span> <span class="error">-120</span> </span><span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>     <span class="dim"><em>File deleted - 120 lines removed</em></span>                                                             <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">↑↓</span> <span class="accent">scroll</span>  <span class="dim">·</span>  <span class="dim">t</span> <span class="accent">split/unified</span>  <span class="dim">·</span>  <span class="dim">click line</span> <span class="accent">comment</span>  <span class="dim">·</span>  <span class="dim">click file</span> <span class="accent">jump</span>  <span class="dim">·</span>  <span class="dim">Esc</span> <span class="accent">finish review</span>                            <span class="accent">unified</span><span class="dim">│</span>
<span class="dim">└───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘</span></pre>

</div>

### Comments and review conversations

Click any diff line (hunk headers are not clickable) to mount an inline comment editor - the same editor the chat draft uses, so clipboard, bracketed paste, dictation bursts and `[Pasted Text N]` blocks behave identically. It wears the unified composer chrome under the clicked line, with `Comment on line N` in the top edge and a dim `Add a comment…` placeholder when empty; saving with <kbd>Enter</kbd> renders a conversation card in place, an empty save cancels.

<details>
<summary>Editor details</summary>

- In unified a click anywhere on the row resolves the line; in split the click column picks old or new. An editor rides with a `Enter save · Esc cancel` hint on its last interior row. A dictated payload's <kbd>Enter</kbd> is buffered as a newline rather than saving the comment, so speech-to-text cannot split a comment in two mid-take.

</details>

A saved comment renders as a **conversation card** - header with the line number and review tag, the state on the right, a rail of turns, `↳ reply`, and `✓ Resolve` / `↺ Reopen`. Resolved cards collapse to a one-line `╰─ ✓ line <N> resolved · <first line>` marker; click it to expand the thread back.

<details>
<summary>Card rules</summary>

- The rail of turns carries a colored dot per turn (you amber, worker blue). Each of your turns carries a dim `✎` and is clickable to rewrite that turn in place; the agent's turns are read-only. `↳ reply` appends a new turn without touching state; a reply never nudges the agent - `↺ Reopen` is the explicit "look again", and reopening also re-nudges the worker.
- Rewriting one of your turns (the `✎` click) rewrites only that turn - an earlier note and any worker replies survive. Clearing a turn and saving removes just that turn; the card is deleted only when no comment of yours would remain, so an orphaned agent reply never lingers. Cancelling restores the card untouched.
- Open, addressed and outdated cards never collapse. The card reports what re-anchoring did to it on a dim row under the turns: `moved from line <N>`, `matched <N> locations, not relocating`, `the code this was on is gone`, or `line changed - resolve, or re-comment on a live line` - the middle two move an open thread to outdated rather than guessing; a resolved thread keeps its state and shows its note only when expanded. The fold-away of an expanded resolved card is keyed on the thread, so a re-anchor that moves the card does not fold it shut.
- `✓ Resolve` applies to open / addressed / outdated; `↺ Reopen` to addressed and resolved - an inapplicable action renders dim and is not clickable, so an addressed card offers both while an open one offers only Resolve.
- The border and rail are neutral grey; color lives on the turn dots and the state label.
- Only the view a thread was authored in writes the anchor notes; another view places the card and reports no note of its own.

</details>

<div class="term">

  <pre class="indent">
                                <span class="dim">│</span>     <span class="dim">4781</span> <span style="background:#033a16">&nbsp;<span class="success">+</span> push_peer_user_turn_into_chat(self, caller);&nbsp;</span>
                                <span class="dim">│</span>         <span class="dim">╭─ </span><span class="bold">💬 line 4781</span> <span class="dim">· R2 ·························</span> <span class="accent">OPEN</span> <span class="dim">─╮</span>
                                <span class="dim">│</span>         <span class="dim">│</span>                                                    <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent">●</span> <span class="accent">you</span>  <span class="dim">✎</span>                                          <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="dim">│</span> does this fire for the failure path too?        <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="subagent">●</span> <span class="subagent">implementer</span>                                     <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="dim">│</span> yes - added a test for that path.               <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent">●</span> <span class="accent">you</span>  <span class="dim">✎</span>                                          <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>    the failure NOTICE path is still unguarded.     <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="dim">moved from line 4763</span>                              <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="dim">↳ reply</span>  <span class="dim">add a note</span>                               <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>                                                    <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent-bold">✓ Resolve</span>   <span class="dim">↺ Reopen</span>                              <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">╰────────────────────────────────────────────────────╯</span>
                                <span class="dim">│</span>         <span class="dim">╭─ </span><span class="bold">💬 line 58</span> <span class="dim">· R1 ·······················</span> <span class="warning">OUTDATED</span> <span class="dim">─╮</span>
                                <span class="dim">│</span>         <span class="dim">│</span>                                                    <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent">●</span> <span class="accent">you</span>  <span class="dim">✎</span>                                          <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>    guard the None case                             <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="dim">matched 2 locations, not relocating</span>               <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="dim">↳ reply</span>  <span class="dim">add a note</span>                               <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>                                                    <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent-bold">✓ Resolve</span>   <span class="dim">↺ Reopen</span>                              <span class="dim">│</span>
                                <span class="dim">│</span>         <span class="dim">╰────────────────────────────────────────────────────╯</span>
                                <span class="dim">│</span>         <span class="success">╰─ ✓ line 88 resolved · </span><span class="dim">rename tok → token</span>
  <span class="dim">↑↓</span> <span class="accent">scroll</span> <span class="dim">·</span> <span class="dim">t</span> <span class="accent">split/unified</span> <span class="dim">·</span> <span class="dim">click line</span> <span class="accent">comment</span> <span class="dim">·</span> <span class="dim">l</span> <span class="accent">reviews</span> <span class="dim">·</span> <span class="dim">Esc</span> <span class="accent">finish review</span></pre>

</div>

<details>
<summary>Comment editor details</summary>

A live dictate take's blip leads the overlay's key-hints bar - a fixed spot that survives the editor row scrolling off-screen; the first <kbd>Esc</kbd> abandons the take (the editor stands; the next one cancels it), and dictated words land in the editor. Saving with <kbd>Enter</kbd> while a take is live abandons it - the user submitted without the words. A truncated take stamps its warning as a notice line at the top of the overlay. Multi-line edits expand the editor inline; the document keeps scrolling around it. Jumping to another file via the rail closes any open editor - it is anchored to a specific line - but saved comments survive the jump. Clipboard paste and dictation land in whichever review editor has focus (the inline editor or the Finish-review overview); a paste over 1000 chars or 5 lines collapses to a `[Pasted Text N - M chars]` chip, expanded again on save. Paste with no editor open does nothing.

</details>

### The review loop

Comments group into numbered **reviews**. Membership lives on each turn, not the thread: the opening comment is sealed by review 1, a later reply by review 2, and the agent's replies by no review. A card header carries a dim `· R{N}` tag for the review the thread first appeared in, or `· unfiled`.

On <kbd>Esc</kbd>, a session with comments that would file gets the **Finish-review modal** instead of closing: the session's comments list, an optional overview, and `[ Submit review ]` sealing the unfiled turns and nudging the agent. A look-only or edit-only session closes directly, minting nothing.

Submitting no longer pastes a markdown bundle into chat: the agent gets a one-line nudge and reads and answers the review through an in-process MCP server. The reviewer re-opens `/diff`, sees each reply as a new turn on the card, and resolves or reopens (which re-nudges the worker).

<details>
<summary>Finish-review and the review MCP</summary>

The modal wears the composer chrome with the comment count in its title and a `➤`-led overview editor. The comments and overview stay out of chat; the agent reads them through the review MCP. A reply on a thread that already belongs to an earlier review counts exactly like a fresh comment; sealing a new turn on a thread the agent had addressed flips it back to open (a resolved thread stays resolved). <kbd>Esc</kbd> dismisses the modal back to the diff.

- `review__list` - the reviews on the caller's project and branch: number, summary, created time, and a per-state tally, newest first. When the branch has none but the project holds reviews on other branches, it names those branches instead of returning an empty list.
- `review__get(review_id)` - the review's overview plus its comments: file, line, side, status, the captured `context` lines of code around the anchor (so a shifted line number still locates the spot), and the thread of turns, each carrying the review number that sealed it.
- `review__reply(comment_id, text)` - append a worker turn and flip the thread Open → Addressed (a resolved comment stays resolved).
- `review__resolve(comment_id)` - mark a comment resolved.

The caller resolves to its project and branch the same way the peers tools do; a `comment_id` outside that scope is rejected, and when that resolution fails the tool names the step that failed (workspace gone, caller in no project, no cwd recorded, checkout not on disk, git reported no branch, detached HEAD) rather than asserting one of them. When the worker's turn ends, forge pings the review's submit origin with one batched system line - `worker addressed review #N - A replied, B resolved, C open. Open /diff.` - rather than one line per call.

</details>

<div class="term">

  <pre class="indent">
<span class="accent-bold">┏━ Finish review · 3 comments ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓</span>
<span class="accent-bold">┃</span>    <span class="dim">· error.rs:88    swallows the decode error, no signal</span>  <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>    <span class="dim">· retry.rs:212   is the backoff actually capped?</span>       <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>    <span class="dim">· parser.rs:41   () on empty input - intended?</span>         <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>                                                           <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span> <span class="accent">➤</span> <span class="dim">Solid overall. Two nits on error handling and one</span>       <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>   <span class="dim">question on the retry path.</span>                             <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>                                                           <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>  <span class="accent-bold">[ Submit review ]</span>     <span class="dim">Ctrl+Enter submit · Esc back</span>       <span class="accent-bold">┃</span>
<span class="accent-bold">┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛</span></pre>

</div>

<kbd>l</kbd> opens the **REVIEWS list** - the branch's submitted reviews, newest first, each with its age, comment count, and a state rollup.

<details>
<summary>REVIEWS list</summary>

A comment whose turns span several rounds is listed under every review it has a turn in; the totals footer counts each comment once. <kbd>↑↓</kbd> move, <kbd>Enter</kbd> jumps the diff to the selected review's first comment, switching to its scope when it is in another commit; <kbd>l</kbd> / <kbd>Esc</kbd> / a click away close. Overviews render only here, never inline in the diff.

</details>

<div class="term">

  <pre class="indent">
 <span class="accent-bold"> REVIEWS</span>                                                <span class="dim">l  close</span>
 <span class="dim">──────────────────────────────────────────────────────────────</span>
  <span class="accent-bold">#3   2h        4 comments   2 open · 1 resolved · 1 outdated</span>
       <span class="dim">Solid overall. Two nits on error handling and one ques...</span>
  #2   1d        2 comments   2 resolved
       <span class="dim">First pass, mostly style.</span>
  #1   3d        5 comments   5 resolved
       <span class="dim">Initial review of the parser rework.</span>
 <span class="dim">──────────────────────────────────────────────────────────────</span>
  <span class="dim">11 comments across 3 reviews  ·  2 open · 8 resolved · 1 outdated</span></pre>

</div>

<details>
<summary>Persistence, re-anchoring, and failure states</summary>

- Review comments persist in the machine-local store per project and branch, each thread carrying its own scope; every save / resolve / reopen / worker reply writes. On reopen a thread re-anchors against a fresh scan: one unambiguous match relocates it - the line plus a recorded neighbour within three lines on each side it has neighbours, at least one neighbour agreeing, whitespace-normalized so a reformat still matches; several matches or none leave the thread outdated in place. Threads auto-delete when their branch is gone - at worktree teardown, or on the next boot for a branch deleted since. A corrupt or unreadable row paints a full-width `review comments failed to load - see logs` notice above the rail rather than reading as an empty review pane.
- Submitted reviews are their own table per project and branch: a 1-based number and optional overview per review, each thread's user turns pointing at the review that sealed it. The `l` list snapshots every thread's current state into per-review rollups on open.
- Empty and failure states, as chat notices instead of an overlay: scanner not run yet ("Git scanner hasn't run yet - try /diff again in a moment."), not a git repo ("Not a git repository."), scanner failure ("Git scanner hit an error - see tracing logs (target: agent.env_git). Try /diff again in a moment." - distinct from not-a-repo because the user IS in a repo; the Inspector paints a sibling yellow "git scanner unhealthy" row), unresolvable default ref ("Branch has changes but the default ref couldn't be resolved (no origin/HEAD, no main, no master). Run `/diff <ref>` with an explicit target."), and clean default ("No changes vs `<name>`, or `No changes vs HEAD.` when the default ref is unknown."). Inside the overlay, a failed hunk scan shows "Scan failed for `<target>` - see tracing logs (target: agent.env_git). Press Esc to retry." in red. A rapid second `/diff` supersedes the first scan's result; a superseded or stale result also drops silently when the user has navigated away from chat or switched session mid-scan.
- In commit mode each scope (commit or "All changes") has its own cached file set; switching resets the scroll and re-tallies the current commit's comment counts. The scan is single-shot on open - no polling.
- Colors: rail status glyphs `M`/`R`/`C`/`T` rust orange, `A` green, `D` red, `U` yellow, `!` red; the rail banner and top-of-viewport marker rust orange; the untracked-suppression line yellow; sticky headers dim caret, bold path, status badge in the status color, `+N` green / `-M` red; gutters and context dim; the "Scan failed" message red; the key-hints bar keys rust orange with dim labels and the mode rust orange; the collapsed resolved marker green with a dim snippet; anchor-note rows dim.

</details>

<details>
<summary>Key-hints bar</summary>

Pinned to the overlay's bottom row: `↑↓ scroll · PgUp/Dn page · t split/unified · click line comment · click file jump · l reviews · Esc finish review`, with the current mode (`unified` / `split`) right-justified. A saved comment adds a `N comments pending` prefix, and the <kbd>Esc</kbd> label reads `finish review` only when this session left a user turn no review has sealed - a fresh comment or a reply on an already-filed thread - and `close` otherwise; resolve and reopen live on each comment box's button row, not on the bar. With an editor open it reads `Enter save · Esc cancel input`.

</details>

### Narrow tier

Below 120 cols the rail hides and the body goes full-width; below 100 the split toggle falls back to unified, which soft-wraps - there is no too-narrow wall.

<div class="term">

  <pre class="indent">
  <span class="accent">▾</span> <span class="bold">crates/forge-tui/src/ui/inspector_pane.rs</span>   <span class="accent">modified</span>      <span class="success">+340</span> <span class="error">-21</span>
<span class="dim">───────────────────────────────────────────────────────────────────</span>
   <span style="color: cyan;">@@ -363,8 +363,15 @@</span>
  <span class="dim">363</span>     fn render_git_section(app: &amp;mut App, lines: &amp;mut Vec&lt;Line&gt;) {
  <span class="dim">364</span>         let banner = "GIT";
  <span class="dim">365</span> <span style="background:#033a16">&nbsp;<span class="success">+</span> let Some(snapshot) = app.active_session()&nbsp;</span>
  <span class="dim">366</span> <span style="background:#033a16">&nbsp;<span class="success">+</span>     .and_then(|s| s.git_diff_snapshot.as_ref())&nbsp;</span>
  <span class="dim">367</span> <span style="background:#033a16">&nbsp;<span class="success">+</span> else { return };&nbsp;</span>
  <span class="dim">370</span>         lines.push(header_row(banner));
  <span class="dim">↑↓</span> <span class="accent">scroll</span>  <span class="dim">·</span>  <span class="dim">PgUp/Dn</span> <span class="accent">page</span>  <span class="dim">·</span>  <span class="dim">t</span> <span class="accent">split/unified</span>  <span class="dim">·</span>  <span class="dim">click line</span> <span class="accent">comment</span>  <span class="dim">·</span>  <span class="dim">click file</span> <span class="accent">jump</span>  <span class="dim">·</span>  <span class="dim">Esc</span> <span class="accent">close</span>  <span class="accent">unified</span></pre>

</div>

<details>
<summary>Commit-mode mockup context</summary>

The stepper mockup shows commit mode on a 5-commit branch: the title and controls rows pinned above the rail, the current commit's message leading the body, and the footer naming the commit keys.

</details>

<div class="term">

  <pre class="indent">
  <span class="accent-bold">COMMITS</span> <span class="dim">·</span> <span class="accent">worker/rate-limit</span> <span class="dim">vs</span> <span class="accent">main</span> <span class="dim">· 5 commits</span>

  <span class="accent">◀</span> <span class="dim">[</span><span class="bold">2 / 5</span><span class="dim">]</span>  <span class="warning">a3f9c1e</span>  <span class="bold">fix the rate-limit threshold check</span>  <span class="accent">▶</span>   <span class="dim">⌄ jump</span>   <span class="dim">·</span>  <span class="accent">● 1 comment so far</span>

  <span class="accent-bold">  FILES · 1</span>            <span class="dim">│</span>  <span class="dim">──────────────────────────────────────────</span>
                         <span class="dim">│</span>  <span class="accent">│</span> <span class="bold">fix the rate-limit threshold check</span>
                         <span class="dim">│</span>  <span class="accent">│</span>
                         <span class="dim">│</span>  <span class="accent">│</span> <span class="dim">The near-threshold check fired on overage too; split it</span>
                         <span class="dim">│</span>  <span class="accent">│</span> <span class="dim">into its own predicate.</span>
                         <span class="dim">│</span>  <span class="accent">▾</span> <span class="bold">app/events/rate_limit.rs</span>  <span class="accent">modified</span>      <span class="success">+8</span> <span class="error">-2</span>
                         <span class="dim">│</span>   <span style="color: cyan;">@@ -65,4 +65,10 @@</span>
                         <span class="dim">│</span>     <span class="dim">66</span> <span style="background:#033a16">&nbsp;<span class="success">+</span> fn is_near_threshold(u: &amp;Update) -&gt; bool {&nbsp;</span>
  <span class="dim">↑↓</span> <span class="accent">scroll</span> <span class="dim">·</span> <span class="dim">◀▶ / [ ]</span> <span class="accent">prev/next commit</span> <span class="dim">·</span> <span class="dim">a</span> <span class="accent">all changes / back</span> <span class="dim">·</span> <span class="dim">j</span> <span class="accent">jump</span> <span class="dim">·</span> <span class="dim">t</span> <span class="accent">split/unified</span> <span class="dim">·</span> <span class="dim">click line</span> <span class="accent">comment</span> <span class="dim">·</span> <span class="dim">Esc</span> <span class="accent">finish review</span>  <span class="accent">unified</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span class="accent">◀</span> <span class="dim">[</span><span class="bold">2 / 5</span><span class="dim">]</span>  <span class="warning">a3f9c1e</span>  <span class="bold">fix the rate-limit threshold check</span>  <span class="accent">▶</span>   <span class="accent-bold">⌄ jump</span>
        <span class="dim">┌──────────────────────────────────────────────────────┐</span>
        <span class="dim">│</span> All changes <span class="dim">(whole branch, one diff)</span>                 <span class="dim">│</span>
        <span class="dim">│</span> <span class="dim">──────────────────────────────────────────────────</span>   <span class="dim">│</span>
        <span class="dim">│</span> <span class="dim">1 · </span><span class="warning">7c1d02a</span> <span class="dim">add the overage helper</span>                   <span class="dim">│</span>
        <span class="dim">│</span> <span class="accent">2 · </span><span class="accent-bold">a3f9c1e fix the rate-limit threshold check</span> <span class="accent">● 1 ◂</span> <span class="dim">│</span>
        <span class="dim">│</span> <span class="dim">3 · </span><span class="warning">e55f210</span> <span class="dim">wire the warning banner</span>          <span class="accent">● 1</span>     <span class="dim">│</span>
        <span class="dim">└──────────────────────────────────────────────────────┘</span>
  <span class="dim">↑↓</span> <span class="accent">move</span>  <span class="dim">·</span>  <span class="dim">enter</span> <span class="accent">go to commit</span>  <span class="dim">·</span>  <span class="dim">Esc</span> <span class="accent">close menu</span></pre>

</div>
