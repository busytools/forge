# Plugins and MCP views

`/plugins` and `/mcp` open their own full-frame views on the shared full-screen page scaffold - the outer titled box, full-width body and footer that [/usage](./usage.md) and [/diff](./diff.md) also render on. No tab header.

## MCP tab

A status-badge summary line - `total` (white-on-black), `connected` (black-on-rust-orange), `needs auth` (black-on-warning), `pending` (black-on-cyan), `disabled` (white-on-dark-gray), `failed` (white-on-error), plus a `refreshing` badge while a refresh is in flight - then an optional red "Last MCP error: ..." line, then one row per server: marker (`>` when selected), name, status badge, scope badge, transport badge, and a one-line dim summary beneath.

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

<details>
<summary>MCP tab details</summary>

Five sub-overlays exist: server details, callback URL, elicitation, auth redirect, plus the marketplace install flow shared with Plugins.

</details>

## Plugins tab

Its own nested tab strip: **Installed · Plugins · Marketplace**, each label `" Tabname (count) "` with the active sub-tab black-on-rust-orange bold and the rest white bold. The Installed and Plugins sub-tabs carry a search field above the list in the unified single-line composer chrome - the query or the dim "Type to filter this list" placeholder inside a one-row thick border, rust orange bold while focused, dim otherwise, the black-on-orange cursor block at the caret; a long query clips rather than growing the region, and a live dictate take blips on the sub-tab strip's left edge. An action row sits between the tab strip and the search field: a black-on-rust-orange bold `Update all (u)` badge, then the auto-update state - `auto-update: off` dim or `auto-update: on` green (the `[plugins] auto_update` switch alone governs). The footer names <kbd>u update all</kbd> and <kbd>c check updates</kbd> while the list holds focus. The Marketplace sub-tab has no action row and renders a blank row there.

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

<details>
<summary>Update runs, markers, rollbacks</summary>

- <kbd>u</kbd> updates every installed plugin in one run (one `claude plugin update` per entry, per-project cwd for project and local scopes); <kbd>c</kbd> checks for updates and applies nothing; <kbd>r</kbd> refreshes the inventory. Both <kbd>u</kbd> and <kbd>c</kbd> render an update report between the search field and the list - a bold header line ("Plugin updates - running..." / the finished summary with "Esc clears") plus one row per plugin naming `plugin@marketplace` and its outcome: updated (to the new version), current, failed (with the CLI's error tail), skipped (auto-update only, with the reason), or update available (check-only, showing old → new). At most 10 rows show before a "...and N more" line; <kbd>Esc</kbd> clears a finished report, and while one is running <kbd>Esc</kbd> closes the pane - the run keeps going in its task and the finished report waits in the pane on reopen.
- Installed rows carry out-of-date markers: every installed row whose marketplace copy reports a different version wears a black-on-warning bold badge naming `installed -> available`, recomputed from the live inventory on every refresh (check runs, <kbd>r</kbd>, the pane-open TTL refresh, installs, updates, rollbacks), so the markers survive the report being cleared or the pane closed and reopened. A run whose post-run refresh failed drops the markers rather than naming versions it could not see.
- With `[plugins] auto_update = true` the update run fires once at forge boot and its report waits in the pane. A failed refresh, check or rollback surfaces its error on the pane's status row; a failed update run reports per-row failures in the update report. Each applied update (previous version, marketplace ref, when, trigger) is recorded in the machine-local store, written by the run task itself so a dropped report event cannot lose the record; the installed-plugin actions overlay grows a "Roll back to previous version" entry for a plugin whose record carries a marketplace ref, restored via the ref, verified against the refreshed inventory, kept on the record if the version did not move, and consumed by a successful rollback.
- Install / actions / add-marketplace each have their own overlay above the Config screen.

</details>

## Config overlays

Modal overlays stacked atop the Config screen when triggered - picking model and effort, output style, language, and the plugin/MCP install and auth flows. Each is a bordered box with its own internal layout. The add-marketplace overlay's input field wears the unified single-line composer chrome: the draft (or the dim "`owner/repo or URL`" placeholder) inside a one-row rust-orange thick border, cursor block at the caret, and a live dictate take blips inside the border - the first <kbd>Esc</kbd> abandons the take, the next closes the overlay, and dictated words land in the field.
