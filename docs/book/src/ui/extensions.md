# Extensions

`/extensions` opens the Extensions page: one full-frame view over everything installable - the plugin tier, each plugin's component kinds, the session's MCP servers and the configured marketplaces. It replaces the retired `/plugins` and `/mcp` commands and renders on the shared full-screen page scaffold - the outer titled box, full-width body and footer that [/usage](./usage.md) and [/diff](./diff.md) also render on.

## Tab bar

Eight flat sibling tabs, each with its live count: **Installed · Skills · Agents · Commands · Hooks · LSP · MCPs · Marketplaces**. The active tab is black-on-rust-orange bold, the rest white bold. There is no second navigation level: each component row carries its source plugin, which preserves the plugin-to-component tier in the row itself.

On the Installed and component tabs, every count describes the tab's INSTALLED rows - the pane keeps the installed stream and the available stream (the marketplace catalog plus the cache leftovers) as two separate row streams, and only the installed one feeds the numbers. MCPs and Marketplaces count their own lists instead. A machine with 19 plugins and 74 skills shows `Installed 19` and `Skills 74`, never the catalog's size.

## Row grammar

Every tab but MCPs and Marketplaces renders one row per extension over the same grammar - state glyph, name, source, installed-vs-available status, badges, action - as aligned columns: the state, name, source and status columns each hold one width for the whole tab (derived from the tab's content, capped), so the rows read as a table. Nothing wraps: a long name, source or status truncates with an ellipsis instead of shifting the columns, and the row's action right-aligns at the pane edge.

- State glyphs and colours: `✓` green for installed, `⚠` warning for an available update, `-` blue for available-not-installed, `✗` red for disabled or failed, and `✓` dim for a plugin installed as an auto-dependency.
- Bracketed badges ride the row where they apply: `[auto-installed]` on a plugin the registry installed as someone's dependency, `[restart required]` on a plugin whose update applied but is not live until a restart consumes it.
- The row's primary action right-aligns at the pane edge: `Update` on a stale row, `Install` on an available-not-installed row. The selected row's gutter takes a `>` marker in place, so the columns never shift; the full action set lives in the Enter overlay.
- Detail rides the row too, truncated before the action column: hook rows name their trigger events (`SessionStart, PreToolUse`), LSP rows state the binary check (`rust-analyzer: on PATH` / `gopls: missing`, resolved against `PATH` once per refresh), plugin rows carry their always-on token cost.
- Component rows show the bare source plugin's name; plugin rows show their marketplace.

## Tabs

- **Installed** - one row per registry-backed plugin, plus its always-on token cost when `claude plugin details` has reported one (fetched once per installed version, then cached). The marketplace catalog never renders here: an available plugin is not an install. A registered plugin renders here even when its own directory ships no components (the LSP-only plugins carry nothing but the server their marketplace manifest declares). A row that cannot load still renders, with its failure reason: an install whose directory is gone, unreadable, not a directory, or absent from the registry entry, or a marketplace whose manifest cannot be read or cannot be parsed.
- **Skills / Agents / Commands** - one row per INSTALLED component of that kind, sourced by its plugin. The available stream's components of the same kind render only behind the Available toggle, dim, after the installed rows.
- **Hooks** - one row per installed plugin hook set with its trigger events.
- **LSP** - one row per server the installed plugins' manifests declare, with the binary check.
- **MCPs** - the MCP page's content unchanged: the status-badge summary line, then one row per server (name, status badge, scope badge, transport badge, dim summary), with the same details overlay and actions the standalone view had.
- **Marketplaces** - one row per configured marketplace: `healthy · N plugins` in green when the manifest loads, otherwise the drift notice (`registry drift - installLocation outside the config dir`), the load failure reason, or a dim `scan pending` when the pane's first disk scan has not landed yet - with the Repair action offered on drift and load failures.

## The Available toggle

The action row carries `Available (a) +N` on the component tabs, where `+N` is the available stream's row count for that tab. Pressing <kbd>a</kbd> appends the available stream's rows after the installed ones: dim names, blue statuses, a plain `Install` action - visually secondary, exactly like the marketplace rows in the catalog. The toggle is per-pane state, off by default, and never touches the Installed tab: that tab draws the installed stream alone, the registry installs plus the load-failure rows, never the catalog.

<details>
<summary>Actions, update-all, and the update report</summary>

- <kbd>Enter</kbd> opens the selected row's overlay: a plugin's actions (Enable / Disable / Update / Roll back to previous version / Install in current project / Uninstall), the install scope picker for an available component's source plugin, or the server details overlay on MCPs. Uninstall always confirms first - the CLI cannot remove one component alone, so the confirm names the whole bundle and its component count ("Removes superpowers and its 14 skills.").
- <kbd>a</kbd> toggles the Available stream on the component tabs; <kbd>u</kbd> updates exactly the stale installed rows - the count in `Update all (u) (N)` on the action row - one `claude plugin update` per entry, per-project cwd for project and local scopes, continuing past failures; <kbd>c</kbd> checks for updates and applies nothing; <kbd>r</kbd> refreshes the inventory.
- The inventory scan reads the plugin registry, the plugin cache, the marketplace manifests, the marketplace registry (`known_marketplaces.json`) and `settings.json` straight off disk. It runs when the page opens and on session change (the pane's five-second TTL), never spawning `claude` - the manual <kbd>r</kbd> and post-action refreshes keep the full CLI path - and first paint renders from the cached snapshot: the pane shows its loading copy while the first scan flies rather than blocking on a CLI spawn.
- The update report renders as it did on the old Plugins view: a bold header plus one row per plugin naming `plugin@marketplace` and its outcome, at most 10 rows before a "...and N more" line, cleared with <kbd>Esc</kbd> once finished. The global top spinner is never driven by extension actions.
- With `[plugins] auto_update = true` the update run fires once at forge boot. Each applied update (previous version, marketplace ref, when, trigger) is recorded in the machine-local store, so the overlay grows "Roll back to previous version" for a plugin whose record carries a marketplace ref, restored via the ref and verified against the refreshed inventory.

</details>

<details>
<summary>Marketplaces and add</summary>

The Marketplaces tab's action overlay carries Update, Remove, and - when the scan flags the marketplace as drifted or failed - Repair, which runs the CLI's remove-and-re-add pair and re-scans. Add marketplace keeps its own overlay, its input field wearing the unified single-line composer chrome: the draft (or the dim "`owner/repo or URL`" placeholder) inside a one-row rust-orange thick border, cursor block at the caret, and a live dictate take blips inside the border - the first <kbd>Esc</kbd> abandons the take, the next closes the overlay, and dictated words land in the field.

</details>
