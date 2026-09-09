# Help

*visible: while `?`-help is active*

Rounded-border panel above the input area, fixed height (`HELP_PANEL_HEIGHT = 14`), max `MAX_ROWS = 10` content rows. Title format: leading "`  Help  `" (RUST_ORANGE bold) + bracketed tabs "`[Keys | Slash | Subagents]`" with the active tab in RUST_ORANGE bold and inactive tabs in DIM (separators "`  |  `" and brackets in DIM) + a hint suffix in DIM. The hint changes per view: "`  (< > switch tabs)`" on the Keys view; "`  (< > tabs  ▲▼ scroll)`" on the Slash and Subagents views.

Three views with different layouts:

- **Keys** - items distributed top-half-left / bottom-half-right. Each cell: `label : description` with label in BOLD and `  :  ` separator in DIM.
- **Slash commands** - single-column two-column list (name + description) via `two_column_list`. Built-in commands: `/config`, `/effort`, `/mcp`, `/plugins`. Plus user-installed commands.
- **Subagents** - same two-column shape as Slash, listing user-defined subagents and their descriptions.

<div class="term">

  <pre class="indent">
  <span class="dim">╭</span> <span class="accent bold">Help</span> <span class="dim">[</span><span class="accent bold">Keys</span><span class="dim"> | </span><span class="dim">Slash</span><span class="dim"> | </span><span class="dim">Subagents</span><span class="dim">]</span>  <span class="dim">(&lt; &gt; switch tabs)</span> <span class="dim">─────────────────╮</span>
  <span class="dim">│</span>                                                                     <span class="dim">│</span>
  <span class="dim">│</span>     <span class="bold">↑ / ↓</span><span class="dim"> : </span>Scroll chat       <span class="bold">Esc</span><span class="dim"> : </span>Cancel current action          <span class="dim">│</span>
  <span class="dim">│</span>     <span class="bold">PgUp / PgDn</span><span class="dim"> : </span>Page         <span class="bold">Ctrl+C</span><span class="dim"> : </span>Interrupt response          <span class="dim">│</span>
  <span class="dim">│</span>     <span class="bold">Ctrl+X</span><span class="dim"> : </span>Expand tool      <span class="bold">?</span><span class="dim"> : </span>Toggle help                    <span class="dim">│</span>
  <span class="dim">│</span>                                                                     <span class="dim">│</span>
  <span class="dim">╰─────────────────────────────────────────────────────────────────────╯</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/help.rs` - `render_keys_help` for Keys, `render_two_column_help` for Slash / Subagents
- **tabs** - switch with `<` / `>` arrows; arrow keys scroll within Slash / Subagents
- **platform-aware bindings** - Input-editing shortcuts swap modifiers per OS - macOS uses `Cmd+Z` / `Cmd+Shift+Z` (undo / redo), `Alt+Left/Right` (word nav), `Alt+Backspace/Delete` (word delete), and `Cmd+C` / `Cmd+V` for copy / paste. Linux + Windows use `Ctrl+` for all of these. `Ctrl+C` still works as fallback copy + interrupt-on-empty everywhere. Constants: `CMD_MOD` / `WORD_NAV_MOD` / `WORD_NAV_MOD_EXCLUDED` in `app/keys.rs`. Reaching the app on macOS requires the kitty enhanced-keyboard protocol (Ghostty / kitty / WezTerm); forge-tui negotiates the relevant flags at startup and again on every resize, since a byte-transparent session manager leaves them on the terminal a reattach left behind.

# Session picker

# Welcome

*visible: appears as the first message in a fresh chat (first turn before any user input)*

Banner is the literal text "**Overview**" in RUST_ORANGE bold. Body is a Ferris-says ASCII art block (in RUST_ORANGE) plus a metadata block (Version, Account, cwd, Session ID) and a single rotating tip. Rendered as a regular message in the scrollback (role `MessageRole::Welcome`) - not a screen overlay. Tip is selected from `WELCOME_TIPS` by `tip_seed % WELCOME_TIPS.len()`. The second metadata line shows `Account: <display_name> · <tier>` when forge-workspace picked the active account from `forge.toml`'s `[[accounts]]`; falls back to `Subscription: <tier>` when forge wasn't launched via the workspace (direct `Agent::spawn` from tests / smoke).

<div class="term">

  <pre class="indent">
  <span class="accent-bold">Overview</span>

  <span class="accent">--------------------------------- </span>
  <span class="accent">&lt; Welcome back to Claude, in Rust! &gt;</span>
  <span class="accent">--------------------------------- </span>
  <span class="accent">        \             </span>
  <span class="accent">         \            </span>
  <span class="accent">            _~^~^~_  </span>
  <span class="accent">        \) /  o o  \ (/</span>
  <span class="accent">          '_ - _' </span>
  <span class="accent">          / '-----' \ </span>


  <span class="dim">Version:      </span><span class="dim">0.14.2 · 7d88141</span>
  <span class="dim">Account:      </span><span class="accent bold">Stargate · team</span>
  <span class="dim">cwd:          ~/Projects/forge</span>
  <span class="dim">Session ID:   550e8400-e29b-41d4-a716-446655440000</span>

  <span class="dim">Tips: Use /mode plan before larger changes, then switch back to code once the plan is clear</span>
</pre>

</div>

- **code** - `crates/forge-tui/src/ui/message.rs::welcome_lines` · ASCII in `FERRIS_SAYS` · tips in `WELCOME_TIPS` (~21 entries)
- **color** - art: RUST_ORANGE · field labels: DIM · account/subscription value: RUST_ORANGE bold · field values + tip: DIM

# Plugins + MCP standalone views

`/plugins` opens `ActiveView::Plugins` and `/mcp` opens `ActiveView::Mcp` - each is its own full-frame top-level view. Both render on the **shared full-screen page scaffold** (outer titled box + full-width body + footer) via `ui::page::render_page` - the same helper [`/usage`](./usage.md) and **`/diff`** use. Config threads a DIM status row + RUST_ORANGE help line through it; the body comes from the view-specific renderer (`ui::config::plugins::render` or `ui::config::mcp::render`). No tab header; no `Config` wrapper title.

## MCP tab

*visible: when the MCP tab is active*

Top: a status-badge summary line - `total N` (white-on-black), `connected N` (black-on-RUST_ORANGE), `needs auth N` (black-on-STATUS_WARNING), `pending N` (black-on-cyan), `disabled N` (white-on-DarkGray), `failed N` (white-on-STATUS_ERROR), and a `refreshing` badge when in-flight. Then optional "Last MCP error: ..." line in STATUS_ERROR. Then per-server list rows: marker (`>` selected / space) + name + status badge + scope badge + transport badge + a 1-line summary in DIM beneath.

<div class="term">

  <pre class="indent">
  <span style="background: white; color: black; padding: 0 4px;">total 4</span>  <span style="background: var(--rust-orange); color: black; padding: 0 4px;">connected 2</span>  <span style="background: var(--status-warn); color: black; padding: 0 4px;">needs auth 1</span>  <span style="background: gray; color: white; padding: 0 4px;">disabled 0</span>  <span style="background: var(--status-error); color: white; padding: 0 4px;">failed 1</span>

  <span class="error">Last MCP error: jetbrains: connection refused at 127.0.0.1:63342</span>

  <span class="bold">&gt; notion</span>     <span style="background: var(--rust-orange); color: black; padding: 0 4px;">connected</span> <span style="background: gray; color: white; padding: 0 4px;">user</span> <span style="background: white; color: black; padding: 0 4px;">stdio</span>
  <span class="dim">  npx -y @notionhq/notion-mcp-server · 8 tools</span>

    context7   <span style="background: var(--rust-orange); color: black; padding: 0 4px;">connected</span> <span style="background: gray; color: white; padding: 0 4px;">user</span> <span style="background: white; color: black; padding: 0 4px;">stdio</span>
  <span class="dim">  npx -y context7-mcp · 4 tools</span>

    claude-ai  <span style="background: var(--status-warn); color: black; padding: 0 4px;">needs auth</span> <span style="background: gray; color: white; padding: 0 4px;">session</span> <span style="background: white; color: black; padding: 0 4px;">remote</span>
  <span class="dim">  https://claude.ai/api/mcp · authenticate to continue</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/config/mcp.rs`
- **overlays** - 5 sub-overlays exist: `render_details_overlay`, `render_callback_url_overlay`, `render_elicitation_overlay`, `render_auth_redirect_overlay`, plus the marketplace install flow shared with Plugins

## Plugins tab

*visible: when the Plugins tab is active*

Has its own nested tab strip: **Installed · Plugins · Marketplace**. Each sub-tab label format is `" Tabname (count) "` (with leading and trailing spaces and the active count in parens). Active sub-tab is black-on-RUST_ORANGE bold; inactive sub-tabs are white bold. Sub-tabs separated by 2 spaces (no separator glyph). The Plugins / Installed sub-tabs include a search field above the list, wearing the unified single-line composer chrome: the query (or the DIM "`Type to filter this list`" placeholder) embedded in a one-row thick border - RUST_ORANGE bold while focused, DIM while not - with the black-on-orange cursor block at the caret; a long query clips rather than growing the region, and a live dictate take blips on the sub-tab strip's left edge so it stays visible whichever option or tab holds the focus. On the Installed / Plugins sub-tabs an **action row** sits between the tab strip and the search field: a black-on-RUST_ORANGE bold `Update all (u)` button badge - the visible affordance for the update-all run - followed by the auto-update state, `auto-update: off` DIM or `auto-update: on` green (`[plugins] auto_update`; the switch alone governs). The footer help line names <kbd>u update all</kbd> and <kbd>c check updates</kbd> while the list holds focus. The Marketplace sub-tab has no action row and renders a blank row there.

Keys on the search-field tabs: <kbd>u</kbd> updates every installed plugin in one run (one `claude plugin update` per entry, per-project cwd for project/local scopes), <kbd>c</kbd> checks for updates and reports without applying anything, <kbd>r</kbd> refreshes the inventory. Both <kbd>u</kbd> and <kbd>c</kbd> render an **update report** between the search field and the list - a bold header line ("Plugin updates - running..." / the finished summary with "Esc clears") plus one row per plugin naming `plugin@marketplace` and its outcome: updated (to the new version) / current / failed (with the CLI's error tail) / skipped (auto-update only, with the reason: the plugin id carries no marketplace) / update available (check-only, showing old → new). At most 10 rows show before a "...and N more" line; <kbd>Esc</kbd> clears a finished report, and while one is running <kbd>Esc</kbd> closes the pane instead - the run keeps going in its task and the finished report is waiting in the pane on reopen. Installed rows also carry **out-of-date markers**: every installed row whose marketplace copy reports a different version wears a black-on-STATUS_WARNING bold badge naming `installed -> available`, kept in `PluginsState.update_availability` and recomputed from the live inventory on every refresh (check runs, <kbd>r</kbd>, the pane-open TTL refresh, installs, updates, rollbacks), so the markers survive the report being cleared or the pane being closed and reopened. A run whose post-run refresh failed drops the markers rather than naming versions it could not see. With `[plugins] auto_update = true` the update run fires once at forge boot against installed plugins, and its report is waiting in the pane. A failed refresh, check or rollback surfaces its error on the pane's status row; a failed update run reports per-row failures in the update report. Forge records each applied update (previous version, marketplace ref, when, trigger) in the redb store, written by the run task itself so a dropped report event cannot lose the record; the installed-plugin actions overlay grows a "Roll back to previous version" entry for a plugin whose record carries a marketplace ref, restored via the ref in the marketplace clone, verified against the refreshed inventory, kept on the record if the version did not actually move, and consumed by a successful rollback.

<div class="term">

  <pre class="indent">
  <span style="background: var(--rust-orange); color: black; font-weight: 700;"> Installed (4) </span>  <span class="bold">Plugins (12)</span>  <span class="bold">Marketplace (2)</span>
  <span style="background: var(--rust-orange); color: black; font-weight: 700;"> Update all (u) </span>  <span class="dim">auto-update: off</span>
  <span class="dim">┏━ Type to filter this list ━━━━━━┓</span>

    <span class="bold">Supabase</span>  <span style="background: rgb(34, 92, 124); color: white; font-weight: 700;"> MCP </span>  <span style="background: var(--status-warn); color: black; font-weight: 700;"> 2.0.9 -&gt; 2.1.0 </span>
    <span class="dim">enabled | user | 2.0.9</span>

    <span class="bold">Pensive</span>  <span style="background: rgb(64, 64, 64); color: white; font-weight: 700;"> SKILL </span>
    <span class="dim">enabled | user | 1.8.0</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span style="background: var(--rust-orange); color: black; font-weight: 700;"> Installed (4) </span>  <span class="bold">Plugins (12)</span>  <span class="bold">Marketplace (2)</span>

  <span class="bold"> Configured marketplaces</span>

    anthropic-official      <span class="dim">Source: github.com/anthropics/claude-plugins</span>
    busytools-internal      <span class="dim">Repo: github.com/busytools/forge-plugins</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span class="bold">Plugin updates</span><span class="dim"> - running...</span>

     pensive@claude-night-market  updating...
     scratch-tools                <span class="dim">skipped</span>  <span class="dim">(plugin id carries no marketplace)</span>
     supabase@claude-plugins-official  <span class="success">updated to 2.1.0</span>
     leyline@claude-night-market  <span class="error">failed</span>  <span class="error">(network unreachable)</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/config/plugins.rs` · sub-tabs in `PluginsViewTab::ALL` · update-run state in `PluginsState.update_run`, rows carried by `SessionUpdate::PluginsUpdateRun*` · persistent check results in `PluginsState.update_availability`
- **marketplace flow** - install / actions / add-marketplace each have their own overlay (rendered atop the Config screen via `render_marketplace_actions_overlay` etc.)

## Config overlays (model picker, output style, language, ...)

*visible: stacked atop the Config screen when triggered*

Modal overlays that draw above the Config screen content. Used for picking model + effort, output style, language, and various plugin/MCP install/auth flows. Each is a bordered box with its own internal layout (see `config.rs:62-79` for the dispatch chain). The add-marketplace overlay's input field wears the unified single-line composer chrome: the draft (or the DIM "`owner/repo or URL`" placeholder) embedded in a one-row RUST_ORANGE thick border, cursor block at the caret, and a live dictate take blips inside the border - the first <kbd>Esc</kbd> abandons the take, the next closes the overlay, and dictated words land in the field.

# Diff viewer

## Inline diff (Edit / MultiEdit / Write)

*visible: every Edit / MultiEdit / Write tool call that carries a diff*

Computed via the `similar` crate's `TextDiff::from_lines`. Optional `[repository]` tag in DIM. Per-hunk header shown in **cyan** with a compacted `@@` form. Each change line carries a marker (`-` red / `+` green) plus a left-padded line number (width auto-sized to the larger of old/new line counts).

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

- **code** - `crates/forge-tui/src/ui/diff.rs::render_diff` · uses `similar` crate's `unified_diff` with default 3-line context
- **colors** - Repository tag: DIM · Hunk header: `Color::Cyan` · Add: `Color::Green` · Delete: `Color::Red` · Context: default fg
- **indenting** - Diff body sits inside the standard tool-call body indent. Each line is the standard body prefix (`   │   ` or `   └─  ` in DIM, 5 cells) followed by `DIFF_BODY_INDENT = "  "` (2 cells), then the diff content - so diff content starts at column 7. Hard tabs in the source expand to spaces at `TAB_WIDTH = 4` column stops measured from the start of the source line, via `ui::wrap::expand_tabs` - shared with the raw unified-diff path and the `/diff` overlay, so all three diff renderers show a tab-indented file (Go) at the same depth as a space-indented one (Rust). That matters because `Span::styled_graphemes` drops control characters: an unexpanded tab measures one column and paints none, so the indent disappears and the wrap budget over-charges for it. Bash stdout that `looks_like_unified_diff` does not recognise (a fragment with `@@` but no file header) renders as plain text and still carries raw tabs. Write-tool diffs cap their length via `cap_write_diff_lines` (head + ellipsis + tail). Markdown files (`.md`) render through the same diff path as code files - no special-case path, no parsed-markdown shortcut.

## Full-screen diff overlay (`/diff`)

*visible: when `/diff` or `/diff <target>` runs, or when the Inspector [GIT section](./inspector.md)'s `🦉` glyph is clicked. Lives in `ActiveView::Diff` - chat / input / projects pane / inspector pane all disappear while it's up, same overlay pattern as **Config** / **Session picker** / **Trust**.*

Click-to-review viewer for changes against any git ref, rendered as **one continuous top-to-bottom scroll** of every changed file in the FILES rail's **folded directory-tree order** (dirs-first, alpha) - one canonical sequence shared by the rail, the body, and the current-file arrow, so scrolling straight down steps the arrow monotonically down the rail (GitHub's "Files changed" model). A single document scroll (`doc_scroll`) flows from the first file to the last. Mouse wheel / <kbd>↑↓</kbd> / <kbd>PgUp/PgDn</kbd> scroll the document; click a file in the FILES rail to jump to it; press <kbd>t</kbd> to flip the whole document between unified (default) and split; click a diff line to attach a comment; click a `┈ ↕ N lines ┈` expander to reveal hidden context; <kbd>l</kbd> opens the **REVIEWS list** of past review passes; <kbd>Esc</kbd> finishes the session's review - a session that left a comment, or replied on one from an earlier round, gets a **Finish-review modal** that seals its turns into a numbered review (with an optional overview) and sends them to the agent, while a look-only session just closes.

The default `/diff` (no args) reads the Inspector [GIT section](./inspector.md)'s snapshot and picks a target accordingly: **worktree dirty** on any branch → `git diff HEAD`; **feature branch clean** → `git diff <default-branch>`. Six distinct empty-state cases each surface a specific system notice in chat instead of opening the overlay: no Inspector snapshot yet ("try again in a moment"), `NoRepo` ("Not a git repository"), scanner crashed (see HIGH-2 distinction below), feature-branch with unresolvable default ref (suggests explicit `/diff <ref>`), and clean default ("No changes vs *&lt;name&gt;*"). `/diff <target>` takes a ref / branch / SHA and runs `git diff <target>` directly - comparing the named ref against the working tree (captures committed + uncommitted in one view, so `/diff main` from a feature branch shows the full PR-equivalent picture).

**Two modes.** When the resolved target has **commits ahead** (`git rev-list <target>..HEAD` non-empty), the overlay opens in commit mode: a stepper walks the commits oldest → newest, each scoped to *just that commit's* diff (`git diff <sha>^..<sha>`, empty-tree fallback for a parentless root commit). You drop your usual line comments per commit; each persists per commit and carries its Open/Resolved state just like a whole-diff comment, and they accumulate across commits so <kbd>Esc</kbd> submits them grouped by commit. **The whole-diff view is a union and a commit's view is a filter over the same store** (the GitHub model): every comment on the branch renders in "All changes" whichever commit it was left against, while a commit shows only what was authored on it. So a rebase or force-push that rewrites the commit a comment was made against cannot put that comment out of reach - it re-anchors against the whole-branch diff like any other. A comment anchored against a *different diff base* is the one thing the union excludes, since its line numbers count against another base. When the target has **no commits ahead** (dirty-tree-only, `/diff HEAD`) it opens in **whole-diff mode** - one continuous diff, byte-identical to before. "All changes" in the jump dropdown returns to that whole-branch view from commit mode. Everything below (FILES rail, diff body, syntax highlighting, comment mini-boxes) is shared; commit mode just adds the stepper bar, the current commit's full message above its diff, the <kbd>◀</kbd>/<kbd>▶</kbd> · <kbd>[</kbd>/<kbd>]</kbd>, <kbd>a</kbd> (toggle current commit ↔ all changes), and <kbd>j</kbd> keys, and per-commit comment scoping.

Two surfaces: a FILES jump rail on the left (15% of width, min 20ch; hidden below 120 cols, where the body goes full-width) with a `│` separator, and the continuous diff body on the right. A **key-hints bar** is pinned to the overlay's bottom row.

- **FILES jump rail:** banner `FILES` + DIM rule + blank row, followed by a box-drawing tree mirroring the [Inspector GIT section](./inspector.md)'s shape - single-child directory chains collapse (`crates/forge-tui/src/app/` renders as one row). Directory rows render the label in DIM with no marker. File leaves carry: connector (`├─`/`└─` DIM), top-of-viewport marker (`▸` in RUST_ORANGE on the file whose document range currently sits at the top of the viewport - the same file the sticky header pins; the body walks files in the rail's order and stashes the message-adjusted top file, so the marker steps monotonically down the rail as the body scrolls instead of jumping around), status glyph (`M`/`A`/`D`/`R`/`C`/`T`/`U`/`!` for Unmerged) + filename + optional `💬 N` badge in RUST_ORANGE when N saved comments are anchored in that file. Click a file leaf → **jump** `doc_scroll` to that file's first row (close any open comment editor, prior_comment preserved per the helper). Click a directory row → no-op. Mouse wheel over the rail advances `rail_scroll`. Untracked files (capped at 4 per scan, each ≤1KB content) appear with the `U` glyph; when the cap is exceeded a yellow `+N untracked suppressed (cap M)` line is appended below the tree.
- **Continuous diff body:** walks the files in the rail's folded-tree order, emitting per file a **banded sticky header** - a filled-background bar (`#1b2130`) spanning the pane width so each file's start reads as a divider: caret `▾` expanded / `▸` collapsed, path in **bold**, status badge `modified`/`added`/`deleted`/... in the status colour, and the file's `+N -M` totals right-justified. The header of the file at the top of the viewport stays **pinned** while that file's body scrolls beneath it. Every file but the last **closes with an end-of-file boundary**: a dim `└─ end <path> ──────` cap row (naming the file that just ended) plus a blank spacer, then the next file's banded header - the cap scrolls up as the new file's band pins. **Context expanders:** where lines are hidden - above a file's first hunk (`┈ ↑ N lines ┈`) or in an inter-hunk gap (`┈ ↕ N lines ┈`) - a dim clickable row shows the hidden count; clicking widens *that file's* shown context (~20 lines/side per click), revealed **in memory** from a wide-context snapshot captured once when the overlay opens (no on-click git), so hunks reveal their surroundings and merge, the row shrinking then vanishing as the gap closes. The collapsed display is derived by narrowing that pinned snapshot, so there's nothing to fetch, fail, or drift mid-review. The pin is bounded (a few thousand lines) so a big file with a small change doesn't emit its whole content; a file whose snapshot would still blow the 8 MiB scan cap renders a bounded diff with a dim `(file too large - context expansion disabled)` note and no expanders, and doesn't blank the rest of the scope. **Unified (default):** one column - `[line-number gutter] [+/-/space sign] [syntax-highlighted text]`, removed-then-added within a hunk, `+` lines on a dark-green tint (`#033a16`), `-` lines on a dark-red tint (`#67060c`), context plain. Long lines **soft-wrap**: continuation rows blank the gutter + sign and align the text under the content column (no horizontal scrolling). **Split (after <kbd>t</kbd>):** the GitHub-style side-by-side view - OLD file (context + removed) left, NEW file (context + added) right, DIM `│` divider between them, each half truncating long lines. Each half is `gutter + 3 + text` wide, and both halves' chrome is reserved before the text columns split what is left, so a row fits the pane exactly. That puts the `│` at `2 + gutter + 3 + left_width + 1` - a column right of the midpoint at odd pane widths, on it at even ones, and independent of gutter width either way. `split_layout` is the one place that computes it, so the painted column and the click boundary cannot drift. Hunk headers (`@@ -X,Y +A,B @@`) render in `Color::Cyan`. **Deleted files collapse** by default to a one-line `File deleted - N lines removed` notice (italic DIM); click the header (or notice) to expand the full body. Click any diff line to mount an inline comment editor (see "Comment workflow"); in unified a click anywhere on the row resolves the line, in split the handler picks old/new by the click column. Saved comments render as conversation cards anchored below their line - a rail of turns with a state header + `✓ Resolve` / `↺ Reopen` actions (see "Branch-persistent review conversations") - in every scope, commit or whole-diff.
- **Syntax highlighting + performance:** in-text colours come from `syntect` (Rust / TS / JS / Python / Go / JSON / TOML / YAML / Markdown / Shell). Two stateful `LineHighlighter` passes per file (old side: context + removed; new side: context + added) preserve multi-line construct state (block comments, multi-line strings) within each side; a context line shows its new-side spans. These spans are computed **once when a file first enters the viewport window and cached unwrapped** - a plain scroll, a <kbd>t</kbd> toggle, and a resize all reuse them, so syntect never re-runs on scroll. Per-file heights are measured lazily (with soft-wrap) for the visible window and cached in `measured_heights` (invalidated on <kbd>t</kbd> toggle, collapse, and width change); off-screen files contribute a cheap line-count estimate to the scrollbar math until scrolled near. Unified renders at any width (it soft-wraps); only **split** needs room, so below `MIN_WIDTH_FOR_SPLIT = 100` cols of body width the split toggle silently falls back to unified - the stored choice returns when the pane widens.
- **Key-hints bar:** pinned bottom row. With no editor open: `↑↓ scroll · PgUp/Dn page · t split/unified · click line comment · click file jump · l reviews · Esc finish review`, with the current mode (`unified` / `split`) right-justified in RUST_ORANGE; a pending-comment count (`N comments pending`) prefixes it when comments are saved. The `l reviews` hint shows only once the branch has a submitted review; the <kbd>Esc</kbd> label reads `finish review` when this session left a user turn no review has sealed - a fresh comment or a reply on an already-filed thread - and `close` otherwise. Resolve / reopen live on each comment box's button row, not the key bar. With an editor open: `Enter save · Esc cancel input`.

<div class="term">

  <pre class="indent">
<span class="dim">┌─ Diff review ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐</span>
<span class="dim">│</span>  <span class="accent-bold">  FILES · 4</span>                   <span class="dim">│</span><span style="background:#1b2130">  <span class="dim">▾</span> <span class="bold">app/diff_overlay.rs</span>   <span class="accent">modified</span>                                                        <span class="success">+42</span> <span class="error">-18</span> </span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────</span>  <span class="dim">│</span><span class="dim">┈┈┈ ↑ 12 lines ┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈</span><span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>   <span style="color: cyan;">@@ -470,7 +470,9 @@</span>                                                                            <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">crates/forge-tui/</span>             <span class="dim">│</span>     <span class="dim">470</span>     pub current_file_idx: usize,                                                         <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">├─</span> <span class="accent">▸</span> <span class="accent">M</span> app/diff_overlay.rs <span class="accent">💬1</span><span class="dim">│</span>     <span class="dim">471</span> <span style="background:#67060c">&nbsp;<span class="error">-</span> pub body_scroll: u16,&nbsp;</span>                                                                <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">├─</span>   <span class="accent">M</span> ui/diff_overlay.rs    <span class="dim">│</span>          <span style="background:#033a16">&nbsp;<span class="success">+</span> /// Scroll across the whole diff doc.&nbsp;</span>                                                <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">└─</span>   <span class="success">A</span> env/git_diff/hunks.rs <span class="dim">│</span>     <span class="dim">472</span> <span style="background:#033a16">&nbsp;<span class="success">+</span> pub doc_scroll: u32,&nbsp;</span>                                                                  <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>     <span class="dim">473</span>     pub comments: Vec&lt;HunkComment&gt;,                                                      <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">╭─ </span><span class="bold">💬 line 471</span> <span class="dim">· unfiled ···········</span> <span class="accent">OPEN</span> <span class="dim">─╮</span>                                          <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent">●</span> <span class="accent">you</span>                             <span class="dim">│</span>                                          <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">│</span>    doc_scroll is the only scroll now  <span class="dim">│</span>                                          <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">│</span>  <span class="accent-bold">✓ Resolve</span>   <span class="dim">↺ Reopen</span>              <span class="dim">│</span>                                          <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>         <span class="dim">╰──────────────────────────────────────╯</span>                                          <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span><span class="dim">└─ end app/diff_overlay.rs ───────────────────────────────────────────────────────────────────────</span><span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>                                                                                                  <span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span><span style="background:#1b2130">  <span class="dim">▸</span> <span class="bold">ui/diff/old_split.rs</span>   <span class="error">deleted</span>                                                        <span class="success">+0</span> <span class="error">-120</span> </span><span class="dim">│</span>
<span class="dim">│</span>                                <span class="dim">│</span>     <span class="dim"><em>File deleted - 120 lines removed</em></span>                                                             <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">↑↓</span> <span class="accent">scroll</span>  <span class="dim">·</span>  <span class="dim">t</span> <span class="accent">split/unified</span>  <span class="dim">·</span>  <span class="dim">click line</span> <span class="accent">comment</span>  <span class="dim">·</span>  <span class="dim">click file</span> <span class="accent">jump</span>  <span class="dim">·</span>  <span class="dim">Esc</span> <span class="accent">finish review</span>                            <span class="accent">unified</span><span class="dim">│</span>
<span class="dim">└───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘</span></pre>

</div>

### Commit stepper (commit mode)

When the target has commits ahead, two rows pin to the top of the overlay above the FILES rail, spaced by a blank line after each: a title row (`COMMITS · <branch> vs <target> · N commits`) and a controls row (`◀ [i / N] <sha> <subject> ▶`, the `⌄ jump` affordance, and a running `● N comments so far` total of the branch's comments, counted once each however many scopes draw them). The FILES rail + diff body below are the same overlay, scoped to the current commit's own diff. <kbd>◀</kbd>/<kbd>▶</kbd> or <kbd>[</kbd>/<kbd>]</kbd> step commits (clamped at the ends); each commit's hunks are scanned lazily on first visit and cached (so a 50-commit branch doesn't scan 50 diffs up front) - a brief `Loading commit diff...` shows while a scan runs. The current commit's full message - subject (bold) then body (DIM, soft-wrapped) behind a RUST_ORANGE `│` rail - leads the diff body above the first file and scrolls with it (a subject-only commit shows just the subject); the body is fetched lazily per commit alongside the hunks (`git show -s --format=%b`). <kbd>a</kbd> toggles between the current commit and the whole-branch "All changes" view without opening the dropdown, remembering the commit so a second <kbd>a</kbd> returns to it. The running total in the stepper replaces the whole-diff mode's `N comments pending` footer prefix.

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

The jump dropdown (<kbd>j</kbd> or click `⌄`) is a GitHub-style commit picker: `All changes` at the top (returns to the whole-branch view), then the commits oldest → newest with per-commit `● N` comment counts and the current commit marked `◂`. <kbd>↑↓</kbd> move the highlight, <kbd>Enter</kbd> jumps to the highlighted scope, <kbd>Esc</kbd> closes the menu (not the overlay). A click on `⌄` toggles it; a click elsewhere closes it.

<div class="term">

  <pre class="indent">
  <span class="accent">◀</span> <span class="dim">[</span><span class="bold">2 / 5</span><span class="dim">]</span>  <span class="warning">a3f9c1e</span>  <span class="bold">fix the rate-limit threshold check</span>  <span class="accent">▶</span>   <span class="accent-bold">⌄ jump</span>
        <span class="dim">┌──────────────────────────────────────────────────────┐</span>
        <span class="dim">│</span> All changes <span class="dim">(whole branch, one diff)</span>                <span class="dim">│</span>
        <span class="dim">│</span> <span class="dim">──────────────────────────────────────────────────</span> <span class="dim">│</span>
        <span class="dim">│</span> <span class="dim">1 · </span><span class="warning">7c1d02a</span> <span class="dim">add the overage helper</span>               <span class="dim">│</span>
        <span class="dim">│</span> <span class="accent">2 · </span><span class="accent-bold">a3f9c1e fix the rate-limit threshold check</span> <span class="accent">● 1 ◂</span> <span class="dim">│</span>
        <span class="dim">│</span> <span class="dim">3 · </span><span class="warning">e55f210</span> <span class="dim">wire the warning banner</span>          <span class="accent">● 1</span>  <span class="dim">│</span>
        <span class="dim">└──────────────────────────────────────────────────────┘</span>
  <span class="dim">↑↓</span> <span class="accent">move</span>  <span class="dim">·</span>  <span class="dim">enter</span> <span class="accent">go to commit</span>  <span class="dim">·</span>  <span class="dim">Esc</span> <span class="accent">close menu</span></pre>

</div>

### Comment workflow

Click on any diff line (added / removed / context - hunk headers are non-interactive) mounts an editor inline below the clicked line. It is the same `InputState` substrate the chat draft uses, so clipboard, bracketed paste, dictation-burst coalescing and `[Pasted Text N]` blocks all behave identically in both places - only the submit key differs per site. The editor wears the same unified composer chrome as the chat draft, indented under the diff line to anchor it: a thick RUST_ORANGE border with `Comment on line N` embedded in the top edge, the ➤ prompt glyph leading the draft, and a caret block at the editor's caret. An empty editor shows a DIM "`Add a comment…`" placeholder after the glyph, and a DIM `Enter save · Esc cancel` hint rides the box's last interior row. While a take is live the pulsing circle blip leads the overlay's key-hints bar - a fixed spot that survives the editor row scrolling off-screen - the first <kbd>Esc</kbd> abandons the take (the editor stands; the next one cancels it), and dictated words land in the editor. Saving with <kbd>Enter</kbd> abandons a live take: the user submitted without the words. A truncated take stamps its warning as a notice line at the top of the overlay. Multi-line edits expand the editor inline; the document keeps scrolling around it via `doc_scroll`.

- <kbd>Enter</kbd> saves the comment, closes the editor, persists the thread to redb, and renders a conversation card in place (header `💬 line <N> · <state>`, an inner rail of turns, a `↳ reply` line, then `✓ Resolve` / `↺ Reopen`) - in every scope, commit or whole-diff. Empty saves on a fresh editor are treated as cancel (no card).
- <kbd>Esc</kbd> with an editor open cancels just the editor; the overlay stays. Without an editor open, <kbd>Esc</kbd> finishes the review: it seals this session's authored comments into a numbered review and dispatches a one-line nudge (`Review #N ready - address it via the review MCP`) to the agent. The comments + overview stay **out of chat** - the agent reads them through `review__get`, not a pasted bundle. An edit-only / look-only close mints nothing and dispatches nothing.
- Each of **your** turns on a saved card is clickable (a dim `✎` marks it): the click reopens the editor seeded with just that turn's text, and the save rewrites *only* that turn - an earlier note and any worker replies survive. The agent's turns are read-only. Clearing a turn and saving removes just that turn (the surviving chain is re-saved); the whole card is deleted only when no comment of yours would remain (so an orphaned agent reply never lingers). Cancelling restores the card untouched.
- A `↳ reply` line at the bottom of the card opens an empty editor that **appends** a new user turn on save - you can add several. A reply never changes the thread's state or nudges the agent; `↺ Reopen` stays the explicit "look again".
- Jumping to another file via the FILES rail closes any open editor - it's anchored to a specific line, and orphaning it on a jump would confuse the geometry. Saved comments survive the jump; only the in-progress editor drops.
- Clipboard paste and speech-to-text dictation both land in whichever review editor has focus - the inline comment editor or the Finish-review overview. Dictation arrives as individual keystrokes rather than a paste event, so it is coalesced by the same burst detector the chat input uses, and an <kbd>Enter</kbd> that belongs to the dictated payload is buffered as a newline instead of saving the comment. A paste over `1000` chars or `5` lines collapses to a `[Pasted Text N - M chars]` chip, expanded again on save. Paste with no editor open is a no-op (nothing for it to land on).

### Branch-persistent review conversations

A saved comment renders as a **conversation card**: a header with `💬 line N · R#` on the left and the state (open / addressed / resolved / outdated) on the right, then an inner rail of turns - a coloured dot per turn (you amber, worker blue) with the text hung off the rail - then a `↳ reply` line and `✓ Resolve` / `↺ Reopen`. Turns are the thread's comments in order, so a worker's `review__reply` shows up as the next turn. Each of **your** turns carries a dim `✎` and is clickable to rewrite that turn in place; the agent's turns are read-only. The `↳ reply` line appends a fresh turn without touching state - it is sealing the next review that hands the thread back to the agent.

<div class="term">

  <pre class="indent">
                                <span class="dim">│</span>     <span class="dim">4781</span> <span style="background:#033a16">&nbsp;<span class="success">+</span> push_peer_user_turn_into_chat(self, caller);&nbsp;</span>
                                <span class="dim">│</span>         <span class="dim">╭─ </span><span class="bold">💬 line 4781</span> <span class="dim">· R2 ································</span> <span class="accent">OPEN</span> <span class="dim">─╮</span>
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
                                <span class="dim">│</span>         <span class="dim">╭─ </span><span class="bold">💬 line 58</span> <span class="dim">· R1 ······························</span> <span class="warning">OUTDATED</span> <span class="dim">─╮</span>
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

The border + rail are neutral grey; colour lives on the turn dots and the state label. `✓ Resolve` applies to open / addressed / outdated (accent, primary); `↺ Reopen` applies to addressed (secondary) and resolved (accent, primary) - an inapplicable action renders dim and isn't clickable, so an addressed card offers both while an open one offers only Resolve. Reopening flips the thread back to Open **and re-nudges the worker** to take another look.

**Resolved cards collapse** to a one-line `╰─ ✓ line <N> resolved · <first line>` marker - green through the tick and the word, the comment's own first line dim after it - resolving is how a comment leaves the working set. Click the marker to expand the thread back to a full card (which is how `↺ Reopen` stays reachable); click the expanded card's header to fold it away again. The expansion is keyed on the thread, not the line, so a re-anchor that moves the card elsewhere doesn't fold it shut. **Open, addressed and outdated never collapse** - addressed is the state with an answer waiting to be read, and outdated is the one flagging that a comment lost its anchor.

**The card says what re-anchoring did to it**, on a dim row under the turns: `moved from line <N>` when the anchor relocated, `matched <N> locations, not relocating` when several places matched equally well, `the code this was on is gone` when none did, and `line changed - resolve, or re-comment on a live line` for a thread already carrying drift from an earlier pass. The middle two move an open thread to outdated where it was rather than guessing. A thread the reviewer already resolved keeps that state, and shows its note only if they expand it.

Only the view a thread was authored in writes any of this. A commit's diff and the whole-branch diff number the same line differently, so another view places the card and reports no anchor note of its own. A thread already carrying drift still shows the standing `line changed` row, which is about the thread rather than about this view.

### Review conversation loop (the review MCP)

Submitting a review no longer pastes a markdown bundle into chat. Instead the agent gets a one-line nudge and reads + answers the review through an in-process MCP server (`mcp__forge__review__*`, auto-approved like the peers / workers tools), turning a review set into a two-way conversation:

- `review__list` - the reviews on the caller's `(project, branch)`: `review_id`, number, summary, `created_at`, and a per-state tally (open / addressed / resolved / outdated), newest first. When the branch has none but the project holds reviews on other branches, it names those branches instead of returning an empty list, so a scope mismatch can't read as "no reviews".
- `review__get(review_id)` - the review's overview plus its comments: `comment_id`, file, line, side, status, the captured `context` lines of code around the anchor (so a shifted line number still locates the spot regardless of scope), and the thread of turns. Each turn carries the `review` number that sealed it (`null` for the worker's own replies), so a comment carried across rounds reads with the earlier exchange as context and the new question identifiable.
- `review__reply(comment_id, text)` - append a worker turn to the thread and flip it `Open → Addressed` (a resolved comment stays resolved). `comment_id` is the thread's v4 UUID.
- `review__resolve(comment_id)` - mark a comment resolved.

The caller resolves to its `(project, branch)` the same way `peers__whoami` does (project via the shared caller-context, branch via a git query on the caller's cwd), so a `comment_id` outside that scope is rejected - one branch's worker can't touch another's review. When that resolution fails, the tool names the step that failed (workspace gone, caller in no project, no cwd recorded, checkout not on disk, git reported no branch, detached HEAD) rather than asserting one of them. When the worker's turn ends, forge pings the review's **submit origin** (the reviewer's session) with one *batched* system line - `worker addressed review #N - A replied, B resolved, C open. Open /diff.` - rather than one line per call. The reviewer re-opens `/diff`, sees each reply as a new turn on the card, and resolves (accept) or reopens (which re-nudges the worker).

### Review sets (chip tags · Finish-review modal · REVIEWS list)

Each session's comments group into a numbered **review** (GitHub-style). Membership lives on each **turn**, not the thread, so one conversation can run through several reviews: the opening comment is sealed by review 1, and a reply the reviewer writes after the agent answers is sealed by review 2, with the agent's replies belonging to no review (they answer a round rather than forming one). A comment's card header carries a dim tag after `💬 line N`: `· R{N}` for the review it *first* appeared in, so the tag stays put as later rounds add turns, or `· unfiled` while nothing has been submitted yet. A thread stored before per-turn membership has its user turns attributed to the review it was filed into when it loads, so its history reads as the round it was part of. Comment state (Open / Addressed / Resolved / Outdated) drives the card; a review's rollup is those states tallied over its member comments at render time, so a worker reply or a later resolve updates the rollup without re-filing.

On <kbd>Esc</kbd> (or the banner `✕`), a session with at least one comment that *would file* - authored this pass and holding a user turn no review has sealed - gets the **Finish-review modal** instead of closing: it lists the session's comments, offers an optional `Overview` cover note, and seals on `[ Submit review ]` / <kbd>Ctrl+Enter</kbd> - minting the review, sealing *this session's* unfiled turns into it, storing the overview on the review, and nudging the agent by number (the overview never enters chat; the agent reads it via `review__get`). A reply on a thread that already belongs to an earlier review counts here exactly like a fresh comment: the new turn needs a round of its own. Sealing a new turn on a thread the agent had already addressed also flips it back to open, since there is a fresh question on it - a thread the reviewer resolved stays resolved. <kbd>Esc</kbd> dismisses the modal back to the diff (keep reviewing). A look-only session, and an edit-only session (every authored turn already sealed), both skip the modal and take the plain close path - neither mints a review nor dispatches anything.

<div class="term">

  <pre class="indent">
<span class="accent-bold">┏━ Finish review · 3 comments ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓</span>
<span class="accent-bold">┃</span>    <span class="dim">· error.rs:88    swallows the decode error, no signal</span>    <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>    <span class="dim">· retry.rs:212   is the backoff actually capped?</span>       <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>    <span class="dim">· parser.rs:41   () on empty input - intended?</span>        <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>                                                             <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span> <span class="accent">➤</span> <span class="dim">Solid overall. Two nits on error handling and one</span>       <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>   <span class="dim">question on the retry path.</span>                              <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>                                                             <span class="accent-bold">┃</span>
<span class="accent-bold">┃</span>  <span class="accent-bold">[ Submit review ]</span>     <span class="dim">Ctrl+Enter submit · Esc back</span>       <span class="accent-bold">┃</span>
<span class="accent-bold">┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛</span></pre>

</div>

The modal wears the unified composer chrome: a thick RUST_ORANGE border with the comment count folded into its title (`Finish review · N comments`), and the overview editor opening with the ➤ prompt glyph - an empty overview shows a DIM "`Overview (optional)…`" placeholder after it. A live dictate take's blip leads the key-hints bar beneath, the first <kbd>Esc</kbd> abandons the take (the modal stands; the next dismisses back to the diff), dictated words land in the overview editor, and submitting with <kbd>Ctrl+Enter</kbd> abandons a live take. The list rows and the `[ Submit review ]` button keep their roles.

Pressing <kbd>l</kbd> opens the **REVIEWS list** - the branch's submitted reviews, newest first, each with its relative age (`2h` / `1d` / ...), member-comment count, and a state rollup (tallied over the review's current comment states, zero counts omitted). A comment whose turns span several rounds is listed under *every* review it has a turn in; the totals footer counts each comment once rather than summing the rows, so it reports how many comments exist. <kbd>↑↓</kbd> move the highlight, <kbd>Enter</kbd> jumps the diff to the selected review's first comment (switching to its scope when it's in another commit), <kbd>l</kbd> / <kbd>Esc</kbd> (or a click away) close back to the diff. It's the only home for review overviews - they never render inline in the diff.

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

### Narrow tier (<120 cols)

The FILES rail collapses on narrow terminals (below the 120-col `rail_width_for` threshold); the continuous body takes the full width, sticky headers and all. Everything else is unchanged - scroll with the wheel / <kbd>↑↓</kbd> / <kbd>PgUp/PgDn</kbd>, <kbd>t</kbd> still flips split/unified, click a diff line to comment, <kbd>Esc</kbd> finishes the review. Below 100 cols of body width (`MIN_WIDTH_FOR_SPLIT`) the split toggle falls back to unified - which soft-wraps and reads fine in one narrow column - rather than blocking with a notice.

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

Six distinct user-facing messages cover every failure / empty-state path:

- `DefaultTarget::NoSnapshot` ("Git scanner hasn't run yet - try /diff again in a moment.") - overlay doesn't open; chat gets the notice.
- `DefaultTarget::NotARepo` ("Not a git repository.") - cwd legitimately isn't inside a git tree.
- `DefaultTarget::ScannerFailed` ("Git scanner hit an error - see tracing logs (target: agent.env_git). Try /diff again in a moment.") - the Inspector scan's rev-parse subprocess hit Failed / Oversize. Distinct from NotARepo because the user IS in a repo; git just couldn't run. The Inspector GIT section paints a sibling yellow "*git scanner unhealthy - see logs (target: agent.env_git)*" row so the failure is visible even without opening /diff.
- `DefaultTarget::NoDefault` ("Branch has changes but the default ref couldn't be resolved (no origin/HEAD, no main, no master). Run /diff &lt;ref&gt; with an explicit target.") - feature branch with no resolvable default ref.
- `DefaultTarget::Clean { default_branch }` ("No changes vs *&lt;name&gt;*." or "No changes vs HEAD." when the default ref is unknown) - clean tree against the resolved default.
- Inside the overlay: when the hunk scanner returns `scanner_ok = false` (Failed / Oversize on the underlying `git diff` calls), the right pane shows ``"Scan failed for `<target>` - see tracing logs (target: agent.env_git). Press Esc to retry."`` in red, embedding the diff target so a typo'd ref is visible at the failure site. Overrides the per-file empty-hunks fallback when files in the rail are partially populated (e.g. only `--no-ext-diff` failed).

The drain pump that lands scan results (`drain_events`) gates on three conditions before opening the overlay: (1) the scan's `seq` must match the latest `App.diff_scan_seq` (a rapid second `/diff` bumps the counter so the older scan's event drops silently as superseded); (2) `active_view == Chat` (user must still be in chat to receive the overlay - navigated-away case drops silently with a DEBUG log); (3) `event.cwd == active_session.cwd_raw` (session-switch mid-scan drops the stale result silently with a DEBUG log). Each skip path emits a DEBUG-level trace log under `APP_SESSION` for triage.

- **code** - `crates/forge-tui/src/ui/diff_overlay.rs::render` (wraps the rail + diff body onto the shared `ui::page::render_page` scaffold via `render_diff_body`; `footer_line` builds the key-hints footer) + `render_stepper` / `render_jump_dropdown` (commit mode) · `crates/forge-tui/src/app/diff_overlay/` (`state.rs` overlay state + `select_scope`, `lifecycle.rs` scan/drain + open/close (`DiffScanKind` event, `spawn_fetch` / `spawn_scope_fetch`), `keys.rs`/`mouse.rs` key + mouse dispatch (`step_commit` / jump nav), `types.rs` shared shapes (`DiffScope`)) · `/diff` slash handler at `crates/forge-tui/src/app/slash/executors.rs::handle_diff_submit` · Inspector `🦉` hit-test at `crates/forge-tui/src/ui/inspector_pane.rs::render_scrollable_body` stamping `PaneHitTarget::InspectorGitOpenDiff` · scanner at `crates/forge-agent/src/env/git_diff/hunks.rs::{scan, scan_commits, scan_commit}` (whole diff / commit list / per-commit diff), each returning a `ScanOutcome { files, scanner_ok, untracked_suppressed }`, reached from the TUI via the `forge_workspace::env::git_diff::hunks` re-export. Durable review threads: wire types in `forge-primitives::review`, redb store in `forge_workspace::store::review` + `Workspace` CRUD, re-anchoring in `forge-agent::env::git_diff::resolver::resolve_anchor`, overlay wiring in `diff_overlay/comments.rs::save_active_input` + `diff_overlay/threads.rs::{hydrate_threads, apply_thread_action}`, teardown cleanup in `spawn.rs::handle_despawn_worker`, gated on `forge-agent::env::worktree::branch_ref_exists`, and the boot sweep for branches deleted since in `Workspace::{sweep_dead_review_branches, start_review_branch_sweep}` over `forge-agent::env::worktree::repo_branch_names` + `store::review::{stored_branches, delete_branch_state}`. Review sets: `ReviewComment.review_id` + the `ReviewSet` type and the `ReviewThread::{is_in_review, origin_review, latest_review, has_unfiled_user_turn}` membership accessors in `forge-primitives::review`, the `reviews` redb table + `load_reviews` / `submit_review` (mint + seal this session's unfiled user turns in one write txn, reopening a thread the new turn hands back) and the legacy attribution in `store::review::lift_thread_membership`, all in `forge_workspace::store::review` + `Workspace`, the Finish-review modal trigger + seal + one-line nudge in `diff_overlay/reviews.rs::{close_with_submit, submit_finish_review, finalize_review_close}`, the `l` list in `diff_overlay/reviews.rs::{toggle_reviews_list, compute_review_rows, compute_review_totals, navigate_to_selected_review}` + `ui::diff_overlay::render_reviews_list`, and the card render via `ui::diff_overlay::{review_tag, render_comment_chip}`. Review conversation: the `mcp__forge__review__*` tools + `ReviewFacade` (prod + mock) in `forge_workspace::mcp::review` (wired into `build_forge_server` for both lead + worker sessions), the `Workspace::review_list / review_get / review_reply / review_resolve` wrappers + store `append_reply` / `find_thread_by_id`, the caller branch resolver `forge_agent::env::git_diff::current_branch`, and the batched turn-end notice via `Workspace::{submit_review origin, review_activity buffer, drain_review_activity}` flushed by `session_task.rs`'s `drain_review_activity_for` on every terminal turn path (the `Message::Result` / `Message::Error` arm, the replaced identity ahead of its rekey, a failed reconnect, and task teardown) → `SessionUpdate::ReviewActivityNotice` → `push_system_message_to_session`. Hard tabs expand at `build_file_highlight` via `ui::wrap::expand_tabs` (4-column stops), and every other control character is swapped there for its Control Pictures glyph (a form feed paints `␌`, ESC `␛`, DEL `␡`, and C1 - which has no picture - the replacement character) via `ui::wrap::replace_control_chars` - both at the single point the unified and the split body read cached spans from. A raw one measures a column and paints none, so a split row under-filled and its `│` landed left of the `divider_col` the click handler splits sides on; a picture is one column wide, so the substitution changes what paints and not what measures.
- **color** - collapsed resolved marker: `╰─ ✓ line N resolved ·` in REVIEW_RESOLVED, the comment snippet after it `DIM` · anchor-note row (`moved from line N` / `matched N locations, not relocating` / `the code this was on is gone`): `DIM` · rail banner (`FILES`): `RUST_ORANGE` bold · banner rule + column separator (`│`): `DIM` · file-rail status glyphs: `M` / `R` / `C` / `T` RUST_ORANGE, `A` green, `D` red, `U` yellow, `!` (Unmerged) red · rail top-of-viewport marker (`▸`): RUST_ORANGE · file path in rail: default fg · "`+N untracked suppressed (cap M)`" line: yellow · sticky file header: caret (`▾`/`▸`) `DIM`, path **bold** default fg, status badge in the status colour (above), `+N` green / `-M` red right-justified · hunk headers (`@@`): `Color::Cyan` · unified/split sign (`+`): green, (`-`): red, context (space) + gutter line numbers: `DIM` · added-line tint `#033a16`, removed-line tint `#67060c`, context plain · collapsed-deleted notice: DIM italic · "Scan failed" body message: red · key-hints bar: keys RUST_ORANGE, labels `DIM`, mode RUST_ORANGE.
- **click** - FILES rail file leaf → jump `doc_scroll` to that file's first row, close any open comment editor. Collapsed-deleted file header / notice → toggle the file's expand (clears its measured height). Diff line → open a `tui_textarea::TextArea` inline below the line (unified: a click anywhere on the row resolves it; split: the click column picks old/new); <kbd>Enter</kbd> saves, <kbd>Esc</kbd> cancels just the editor. On a saved `💬` card: clicking one of **your** turns (dim `✎`) reopens the editor seeded with that turn's text and rewrites only it; the agent's turns are read-only; the `↳ reply` line opens an empty editor that appends a new turn. A durable thread's card also carries `✓ Resolve` / `↺ Reopen` actions; clicking an applicable one flips that thread's state (persisted to redb) - an addressed card offers both, and reopening re-nudges the worker. A collapsed resolved marker expands its thread and the header of an expanded resolved card folds it back, both keyed on the thread so a re-anchor moving the card does not change what a click does. <kbd>t</kbd> → flip unified ↔ split. <kbd>↑↓</kbd> / <kbd>PgUp/PgDn</kbd> / wheel over body → advance `doc_scroll`; wheel over the rail → `rail_scroll`. <kbd>Esc</kbd> with no editor open → when this session authored a comment, open the Finish-review modal (its `[ Submit review ]` button / <kbd>Ctrl+Enter</kbd> seals the review + nudges the agent to address it via the review MCP; <kbd>Esc</kbd> dismisses back to the diff); a look-only session closes directly. <kbd>l</kbd> → open the REVIEWS list (<kbd>↑↓</kbd> move, <kbd>Enter</kbd> jump to a review's first comment, <kbd>l</kbd> / <kbd>Esc</kbd> / a click-away close). Inspector pane's `🦉` glyph → enter overlay via `open_default` (auto-detected target). Commit mode only: <kbd>◀</kbd>/<kbd>▶</kbd> or <kbd>[</kbd>/<kbd>]</kbd> step commits, <kbd>a</kbd> toggles the current commit ↔ the whole-branch diff (remembering the commit), <kbd>j</kbd> or a click on `⌄ jump` toggles the dropdown; in the dropdown <kbd>↑↓</kbd> move, <kbd>Enter</kbd> jumps, <kbd>Esc</kbd> / a click-away closes the menu (leaving the overlay open).
- **data source** - `forge_agent::env::git_diff::hunks::scan(cwd, target)` runs three subprocess calls per invocation: `git diff <target> --name-status` (file list with M/A/D/R/C/T/U classification), `git diff <target> --no-ext-diff` (full unified diff content - `--no-ext-diff` defeats user-configured difftastic / delta so the parser sees standard format), and `git ls-files --others --exclude-standard` (untracked files, only when `target == "HEAD"`; capped at `MAX_UNTRACKED_FILES=4`, each read up to `MAX_UNTRACKED_FILE_SIZE=1024` bytes). Returns `ScanOutcome { files, scanner_ok, untracked_suppressed }` - `scanner_ok` is `false` when any subprocess hit Failed / Oversize so the renderer surfaces "Scan failed" instead of an ambiguous empty pane; `untracked_suppressed` carries the cap-overflow count for the rail's yellow notice. Files keep `git --name-status` order, which is the document order. Unknown `--name-status` codes (X, B) and malformed `@@` headers fire WARN logs under `agent.env_git` and drop the entry. Single-shot scan on overlay open - no polling. **Commit mode** adds `scan_commits(cwd, target)` (`git log --reverse --format=… <target>..HEAD` → the ordered `CommitMeta` list) plus `scan_commit(cwd, sha)` per commit (`git diff <sha>^..<sha>`, empty-tree range for a root commit); the commit list is scanned on open, each commit's hunks lazily on first visit. Comments carry a `commit: Option<String>` sha so navigating between commits keeps every comment but only the current scope's render and count.
- **scope** - Rail shown at ≥120 cols (15% of width, min 20ch; matches the `rail_width_for` threshold) with the body filling the rest; below 120 the rail hides and the continuous body goes full-width; below 100 cols of body width (`MIN_WIDTH_FOR_SPLIT`) the split toggle falls back to unified (which soft-wraps) - there is no "too narrow" wall. The whole document shares one scroll (`doc_scroll`); per-file heights + highlight spans are cached lazily for the visible window. In commit mode each scope (commit or "All changes") has its own cached file set - switching resets `doc_scroll` and re-tallies the current commit's comment counts. Persisted state: review comments in every scope (commit and whole-diff) are **redb-backed** per `(project name, branch)`, each thread carrying its own scope - written on every save / resolve / reopen / worker reply, re-anchored against a fresh scan on reopen (one unambiguous match relocates - the line, plus one of its recorded neighbours within three lines on each side it has neighbours, and at least one neighbour must agree, so a line alone in its hunk never relocates; compared with whitespace normalised so a `cargo fmt` reflow still matches; several matches or none leaves the thread outdated in place rather than guessing), and auto-deleted once their branch is gone - at worktree teardown, or on the next boot for a branch deleted since (see "Branch-persistent review conversations" above). Only the in-progress editor stays transient; the Finish-review submit seals the review + nudges the agent, and the durable threads (with any worker replies) persist independently. If loading a branch's threads fails (a corrupt / unreadable redb row), a full-width `review comments failed to load - see logs` notice paints above the rail/body so the failure never reads as an empty review pane. Submitted reviews are their own redb table per `(project name, branch)` (the `reviews` table alongside `review_threads`): each `ReviewSet` carries a 1-based number + optional overview, and each of a thread's user turns points at the review that sealed it - so one thread can be a member of several reviews, and a sealed turn never moves. The `l` list snapshots every thread's current state into per-review rollups on open; reviews and their overviews live only there, never inline in the diff.

# Error states

Forge does not have dedicated full-screen error views (apart from the Config-screen-loading errors above). All error / failure UI is delivered as **system-message notices** pushed into the chat scrollback (see [Chat / system notice](./chat.md)) at one of three severities. The chat scrollback is the only error surface; on a fatal connection error the input area is locked with a hint instead of being replaced.

## Connection failed

*visible: when `spawn_session` can't start (bad path, missing binary, transport error) or the bridge later detects subprocess exit*

Pushed via `push_connection_error_message`. Body is the literal string `"Connection failed: <error>\n\nInput disabled after an error. Press Ctrl+Q to quit and try again."` rendered as a system message. App status flips to `AppStatus::Error`; the input area becomes read-only.

<div class="term">

  <pre class="indent">
  <span class="error">Connection failed: claude binary not found at /usr/local/bin/claude</span>

  <span class="error">Input disabled after an error. Press Ctrl+Q to quit and try again.</span></pre>

</div>

- **code** - `crates/forge-tui/src/app/events/session.rs::push_connection_error_message`
- **side effects** - session id cleared, account cleared, MCP state reset, usage reset, pending submits cleared, status → Error

## Settings parse error

*visible: when `~/.claude/settings.json` (or a project settings file) can't parse*

Pushed as a Warning-severity system notice. Rate-limited via `SessionUpdate::SettingsParseError` deduping (one notice per file, per parse attempt). Wording specifies the file path and the parser's hint about location.

<div class="term">

  <pre class="indent">
  <span class="warning">Failed to parse ~/.claude/settings.json: expected `,` at line 42 column 5. Falling back to defaults.</span></pre>

</div>

## Rate limit notice

*visible: when wire reports `RateLimitStatus::AllowedWarning` or `Rejected`*

System notice - Warning severity for `AllowedWarning`, Error severity for `Rejected`. Body comes from `format_rate_limit_summary` (in `app/events/rate_limit.rs`). Three message branches:

- **Org-level disabled extra usage** (Rejected + no primary context + `overage_disabled_reason == "org_level_disabled"`) → "*Extra usage credit is required to continue. Use /extra-usage to enable it, /model to switch models, or wait for the rate-limit window to reset.*"
- **Near threshold without overage** (AllowedWarning + `is_using_overage = false` + `surpassed_threshold > 0`) → "*Near rate-limit threshold. Resets in 4h 23m at 14:30 UTC.*"
- **General case** → "*{Approaching | Rate limit reached}, you've used N% of your &lt;type&gt; rate limit. {You are using your overage allowance. | You can continue using your overage allowance.} Resets in &lt;timer&gt;.*"

This same message also tints the rate-limit chip on the assistant's reply when applicable.

- **code** - `crates/forge-tui/src/app/events/rate_limit.rs::format_rate_limit_summary`

## Tool-use error (per-tool failure)

*visible: appended as the body of a failed tool call*

Failed tool calls render with the `✗` status icon (red). For standard rows, the error message body comes from `render_tool_use_error_content` - every non-empty line of the message gets `STATUS_ERROR` colour (the entire body is red, not just the first line). For Bash specifically, failure renders inside the bordered card body via `failed_execute_first_line` - only the first non-empty stderr line shown, in `STATUS_ERROR`. `looks_like_internal_error` payloads get a different treatment via `render_internal_failure_content`: a bold red "Internal Agent SDK error" header followed by an `summarize_internal_error` body line in red.

<div class="term">

  <pre class="indent">
  <span class="error">✗</span> <span class="bold">⬚</span> crates/forge-tui/src/missing.rs
  <span class="dim">│  </span><span class="error">File not found: crates/forge-tui/src/missing.rs</span>
  <span class="dim">└─ </span><span class="error">error path: open() returned ENOENT</span>

  <span class="error">✗</span> <span class="bold">▶</span> cargo build --release
  <span class="dim">└─ </span><span class="error">error: failed to compile due to 3 errors</span></pre>

</div>

Internal-error variant (timeouts, panics, SDK protocol violations):

<div class="term">

  <pre class="indent">
  <span class="error">✗</span> <span class="bold">⬚</span> crates/forge-tui/src/something.rs
  <span class="dim">│  </span><span class="error bold">Internal Agent SDK error</span>
  <span class="dim">└─ </span><span class="error">stream closed at offset 12483 mid-frame</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/tool_call/errors.rs::render_tool_use_error_content` · Bash-specific stderr extraction in `execute.rs::render_execute_content` + `errors.rs::failed_execute_first_line` · internal-error variant in `render_internal_failure_content`

## Slash-command error

*visible: when a slash command fails (bad arguments, unknown command, etc.)*

System notice with Error severity. Same shape as the connection-failed message minus the input-lock side effect.

# Theme tokens

Hardcoded in `crates/forge-tui/src/ui/theme.rs`. No light mode, no custom themes, no YAML config. Anything not on this list is rendered with ratatui's `Color::White` default fg + terminal-default bg.

| Token | CSS variable | Value |
|---|---|---|
| RUST_ORANGE | `--rust-orange` | `Rgb(244, 118, 0)` |
| DIM | `--dim` | `Color::DarkGray` |
| USER_MSG_BG | `--user-msg-bg` | `Rgb(40, 44, 52)` |
| STATUS_ERROR | `--status-error` | `Color::Red` |
| STATUS_WARNING | `--status-warn` | `Color::Yellow` |
| SLASH_COMMAND | `--slash` | `Color::LightMagenta` |
| SUBAGENT_TOKEN | `--subagent` | `Color::LightBlue` |

Plus inline `Color::White` (default fg) and `Color::Yellow` scattered through code (e.g. `build_primary_line`'s `?` in white). Borders and dividers use `theme::DIM`. Card borders use `theme::DIM`.

# Glyphs in use

Inventory of every Unicode character used as chrome - borders, icons, status, separators.

| Glyph | Codepoint | Where | Notes |
|---|---|---|---|
| ╭ | U+256D | autocomplete, help, settings panes (rounded box top-left) | via `BorderType::Rounded` on the wrapping `Block` |
| ╮ | U+256E | top-right corner | |
| ╰ | U+2570 | bottom-left corner | |
| ╯ | U+256F | bottom-right corner | |
| ─ | U+2500 | horizontal lines | also `theme::SEPARATOR_CHAR` (used between chat and input, banner rules, Inspector pane GIT/TASKS separator, etc.) |
| │ | U+2502 | vertical lines | standard tool body prefix (`   │   `) · overlay borders · Inspector GIT tree continuation column |
| ├ | U+251C | Inspector GIT tree connector | tee-right - marks a non-last child in the file tree, combined with `─` as `├─ ` |
| └ | U+2514 | tool body last-line corner · Inspector GIT tree connector | combined with `─` as `└─ ` for the final body row, also the last-child connector in the GIT file tree |
| ⬚ | U+2B1A | Read tool icon | open square - read-only inspection |
| ▣ | U+25A3 | Write/Edit/MultiEdit/NotebookEdit/Delete | filled square - mutation |
| ⌕ | U+2315 | Glob/Grep/LS | magnifier |
| ▶ | U+25B6 | Bash | black right-pointing triangle - execute. Used in the bordered Bash card title (RUST_ORANGE) |
| ◇ | U+25C7 | Task/Agent (Subagent) | diamond - delegated worker |
| ⊕ | U+2295 | WebFetch/WebSearch (in-process and server-tool variants) | circled plus - fetch from outside; same glyph used for the server-tool wire names `web_fetch` and `web_search` so the card chrome stays stable regardless of which side of the wire the call comes from |
| ⊙ | U+2299 | EnterPlanMode/ExitPlanMode/Config | circled dot - meta op |
| ⇄ | U+21C4 | Move/EnterWorktree/ExitWorktree | bidirectional arrows - directory + worktree transitions |
| ◆ | U+25C6 | Workflow | filled diamond - agent-script flow (distinct from Task/Agent's hollow ◇) |
| ◉ | U+25C9 | TaskOutput · Monitor | fisheye - read-only task observability (Monitor reuses the backgrounded-task render pattern; TaskOutput is the read-only inspector of an in-flight task) |
| ◍ | U+25CD | TaskStop | circle with vertical fill - terminate a Monitor / Workflow / scheduled task (paired with TaskOutput / Monitor like KillBash with Bash) |
| ⏲ | U+23F2 | ScheduleWakeup | timer-clock - schedule a one-shot wake-up at a future timestamp |
| * | U+002A | CronCreate · CronDelete · CronList | asterisk - Cron-family scheduled-recurring (cron-syntax mapping `* * * * *`; ASCII width-1 keeps the kind-icon column deterministic across terminals) |
| ◈ | U+25C8 | Gotify glyph (Inspector status + [chat notification block](./peers.md)) | white diamond containing black diamond - the shared Gotify icon: the Inspector GOTIFY header status while the stream is connected (RUST_ORANGE; a dropped stream swaps in `⚠`) and the inbound notification chat-block kind-icon (GOTIFY cyan); distinct from the ◇ Task / ◆ Workflow diamonds |
| ⚠ | U+26A0 | degraded-but-not-broken state · Projects-pane `AuthRequired` · Inspector GOTIFY header when the stream is down | warning sign - STATUS_WARNING on the surfaces above, marking a state the user should notice but that is not a failure; the red `✗` / `✕` stay reserved for something actually broken. **One exception, and it is deliberate:** a `Bailed` account renders this glyph in STATUS_ERROR on [preflight](./preflight.md) and on the launchpad's account row alike (`preflight::account_glyph`, which both call). On the one screen that can stop forge starting, mid-flight and failed must not differ only by glyph - and the project row's own account chip was already red for the same state. The GOTIFY header borrows the pairing from `glyph_for_lifecycle`'s `AuthRequired` rather than minting a new glyph, since the diamond family (`◇` Task, `◆` Workflow, `◉` TaskOutput, `◍` TaskStop) is fully claimed |
| ✦ | U+2726 | Skill · Advisor | four-pointed star - meta capability (skill invocation); also the server-tool wire name `advisor`, which surfaces model-side counsel without a local handler |
| ⌖ | U+2316 | ToolSearch (in-process and server-tool variants) | position-indicator - sibling of magnifier ⌕ for searching among available tools; same glyph used for the server-tool wire names `tool_search_tool_regex` and `tool_search_tool_bm25` |
| ▲ | U+25B2 | PushNotification | up-pointing triangle - outbound signal (push to user) |
| ⇨ | U+21E8 | RemoteTrigger | rightward arrow - fire-and-forget remote trigger |
| ⚙ | U+2699 | LSP | gear - tooling integration (language server protocol calls) |
| ◈ | U+25C8 | MCP-server line in an L2 group summary (`mcp__<server>__*`) | diamond-in-square - marks an external MCP-server call apart from local tools; only appears in the grouped summary, keyed per server |
| ○ | U+25CB | fallback tool icon · todo Pending · permission/plan-approval unfocused · Projects pane sleeping-project row glyph | open circle - reused across multiple semantics (single code point) |
| ✓ | U+2713 | completed status · permission Allow icon · todo Completed | `theme::ICON_COMPLETED` |
| ✗ | U+2717 | failed status · permission Reject icon · MCP SERVERS failed server | `theme::ICON_FAILED` |
| ➤ | U+27A4 | input prompt · SendMessage | `theme::PROMPT_CHAR`; also the SendMessage tool glyph per `theme::tool_name_label` |
| ▁▂▃▄▅▆▇█ | U+2581-2588 | dictate level meter | block ramp - the 26-cell meter in the composer's [status row](./input.md) while recording: DIM at or under the take's silence-floor gate, grading dim - orange - hot above it, and frozen dim-to-blue while transcribing. The old top-border indicator cells are gone; idle reserves nothing. |
| ● | U+25CF | dictate recording dot · MCP SERVERS connected server · `/dictate` in-force marker | filled circle leading the composer's [status row](./input.md) while a take records, in RUST_ORANGE pulsing on a 1.05 s cycle (steady under reduced motion); on the [MCP SERVERS](./inspector-processes.md) name line it marks a connected server, steady, in green rgb(130,199,107); in the [dictate overlay](./pickers.md) it marks the value in force, steady, in RUST_ORANGE - the session pick in device mode, the override-or-default row in the options body |
| ◌ | U+25CC | dictate transcribing dot · MCP SERVERS pending server | dotted circle leading the status row while transcription is in flight, in handoff blue rgb(97,160,224), same pulse; on the MCP SERVERS name line it marks a pending server, steady, in the same blue |
| · | U+00B7 | permission-prompt option separator · Projects pane Sleeping / Failed / LoggedOut row glyph | middle dot - used to space the inline options in `render_permission_lines` (`"  ·  "`, DIM); also the lifecycle glyph on a Projects-pane project / worker row whose session is Sleeping, Failed or LoggedOut (DIM) |
| • | U+2022 | plan-approval pre-approved actions list · `/account` current-account marker | bullet - used in plan-approval prompt's optional pre-approved actions block (DIM); also marks the account a session is currently running under in the [account-switch overlay](./pickers.md) |
| ▸ | U+25B8 | `/dictate` overlay highlight cursor | black right-pointing small triangle - RUST_ORANGE beside the highlighted row in the [dictate overlay](./pickers.md); the highlighted label itself goes white bold |
| ◆ | U+25C6 | Inspector WORKFLOWS entry header | black diamond - prefixes each [WORKFLOWS](./inspector.md) entry's `meta.name` header row (bold while running, DIM once collapsed) |
| △ | U+25B3 | Projects pane / NEEDS ATTENTION pending-prompt glyph | white up-pointing triangle - background session paused on a permission prompt or question; STATUS_WARNING yellow on a Projects-pane row and in the Inspector NEEDS ATTENTION band |
| ◆ | U+25C6 | Projects pane turn-completed-unseen glyph | filled diamond in COMPLETION green (`theme::COMPLETION` = `REVIEW_RESOLVED`) on a Projects-pane project / worker row whose session's last turn completed while it was not the active tab; replaces the idle `●` until the session is opened, then clears. Informational only - spinner, `△`, `⚠` and `✕` outrank it. Third use of the filled diamond, after the Workflow tool icon and the Inspector WORKFLOWS entry header; the glyph column position and green colour separate it from those |
| ▤ | U+25A4 | Narrow-tier Projects top-bar icon | square with horizontal fill - single-cell pane icon at the top of the chat area; toggles the Projects overlay on click (DIM when closed, RUST_ORANGE bold when open) |
| ✕ | U+2715 | Failed-turn / failed-worker glyph | multiplication x - STATUS_ERROR red on a Projects-pane row and in the Inspector NEEDS ATTENTION band when a session's turn died (or a worker's spawn failed). Distinct from the yellow `△`: an error is not a request for input. |
| ✕ | U+2715 | Narrow-tier overlay close glyph | multiplication x - Projects / Inspector pane: sits at the right edge of the overlay banner; click dismisses the overlay without switching session (DIM). |
| 💬 | U+1F4AC | **Diff overlay** comment card + per-file badge | speech balloon - the header of a saved per-line conversation card (rendered after its anchor diff line, `💬 line <N> · R#`) and rail badge "`💬 N`" next to files that have N pending comments. The card border + rail are neutral grey; the state label on the header right carries the colour: RUST_ORANGE open, blue addressed, green resolved, yellow outdated. The rail badge stays RUST_ORANGE. The same glyph carries the reviewer-side waiting signal outside the overlay: a `💬 N` badge in REVIEW_ADDRESSED on the [Inspector GIT header](./inspector.md) (active session, beside the `🦉`) and a `💬` row glyph in the same colour in the [NEEDS ATTENTION band](./inspector.md) (background sessions), both counting worker answers nobody has come back to. |
| ✎ | U+270E | **Diff overlay** comment card - editable-turn marker | DIM pencil hung after the `you` label on each of your own turns; the turn is clickable to rewrite it in place. Agent turns carry no pencil (read-only). |
| ↳ | U+21B3 | **Diff overlay** comment card - reply line | DIM `↳ reply` line below the turns; clicking it opens an empty editor that appends a new user turn (no state change, no nudge). |
| ? | U+003F | question-prompt header marker · help toggle key | RUST_ORANGE in question header; same key activates help |
| \| | U+007C | question-prompt horizontal-options separator | ASCII pipe used between options in the horizontal question layout (`"  \|  "`, DIM) - sits next to box-drawing `│`; flagged in #40 for replacement |
| [ ] | U+005B / U+005D | Projects-pane status panel mode badge · multi-select checkbox | ASCII brackets wrap the panel's `Mode` badge in the pane footer; the multi-select checkbox in question prompt uses `[x]` / `[ ]` |
| ▓▓▓▓░░░░ | U+2593 / U+2591 | Projects-pane status panel usage bars | filling bar for the `Ctx`, `5h`, `7d` rows, and the `cap` row on an API-billed account, stretched to the pane width by `bar_cells_for`. Filled cells (`▓`) take their colour from the cell's position in the bar - four left-to-right zones green / yellow / orange / red, not a utilization threshold; empty cells (`░`) DIM |
| ⎇ | U+2387 | Inspector-pane GIT section branch marker | alternative-key symbol prefix for the branch row in the right-hand [Inspector pane](./inspector.md)'s GIT section (DIM); branch label DIM on the default branch, RUST_ORANGE on a feature branch, yellow (`HEAD`) when the snapshot reports detached HEAD |
| ⠋ ⠙ ⠹ | U+280B... | spinner frames · Projects pane Running / Spawning row glyph | Braille spinner during in-progress states (10 frames in `SPINNER_FRAMES`); also the animated glyph on a Projects-pane project / worker row whose session has a turn in progress or a live backgrounded task (RUST_ORANGE on the focused row, terminal default elsewhere) |
| ▸ | U+25B8 | selection indicator | permission / question / plan-approval options · todo InProgress · autocomplete selected row |
| > | U+003E | trust prompt selected action · MCP server-list selected marker | ASCII greater-than (white-on-RUST_ORANGE bold for selected, leading space for unselected) |

*The source document highlights outlier rows in warning colour.*

*Scope: **current state only** - anything new lands here in the same PR that lands the code.*
