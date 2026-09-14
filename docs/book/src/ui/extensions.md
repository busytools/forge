# Extensions

`/extensions` opens the Extensions page: one full-frame view over everything installable - the plugin tier, each plugin's component kinds, the session's MCP servers and the configured marketplaces. It replaces the retired `/plugins` and `/mcp` commands and renders on the shared full-screen page scaffold - the outer titled box, full-width body and footer that [/usage](./usage.md) and [/diff](./diff.md) also render on.

## Tab bar

Eight flat sibling tabs, each with its live count: **Installed · Skills · Agents · Commands · Hooks · LSP · MCPs · Marketplaces**. The active tab is black-on-rust-orange bold, the rest white bold. There is no second navigation level: each component row carries its source plugin, which preserves the plugin-to-component tier in the row itself.

On the Installed and component tabs, every count describes the tab's INSTALLED rows - the pane keeps the installed stream and the available stream (the marketplace catalog plus the cache leftovers) as two separate row streams, and only the installed one feeds the numbers. MCPs and Marketplaces count their own lists instead. A machine with 19 plugins and 74 skills shows `Installed 19` and `Skills 74`, never the catalog's size.

## Row grammar

Every tab renders its rows over the same grammar - state glyph, name, source, status, badges, and, where a row has one, a right-aligned action - as aligned columns: the state, name, source and status columns each hold one width for the whole tab (derived from the tab's content, capped), so the rows read as a table. Nothing wraps: a long name, source or status truncates with an ellipsis instead of shifting the columns, and the row's action right-aligns at the pane edge.

- State glyphs and colours: `✓` green for installed, `⚠` warning for an available update, `-` blue for available-not-installed, `✗` red for disabled or failed, and `✓` dim for a plugin installed as an auto-dependency.
- Bracketed badges ride the row where they apply: `[auto-installed]` on a plugin the registry installed as someone's dependency, `[restart required]` on a plugin whose update applied but is not live until a restart consumes it.
- The row's primary action right-aligns at the pane edge: `Update` on a stale row, `Install` on an available-not-installed row. The selected row's gutter takes a `>` marker in place, so the columns never shift; the full action set lives in the Enter overlay.
- Detail rides the row too, truncated before the action column: hook rows name their trigger events (`SessionStart, PreToolUse`), LSP rows state the binary check (`rust-analyzer: on PATH` / `gopls: missing`, resolved against `PATH` once per refresh), plugin rows carry their always-on token cost.
- Component rows show the bare source plugin's name; plugin rows show their marketplace.

## Scrolling

Every tab's list scrolls. The list renders a window of rows starting at the tab's scroll offset, and the offset exists to keep the selection visible: <kbd>Up</kbd>/<kbd>Down</kbd> move the selection one row and the window follows when the selection would leave it, <kbd>PageUp</kbd>/<kbd>PageDown</kbd> move the selection by a page, and <kbd>Home</kbd>/<kbd>End</kbd> jump to the first and last row. The selection can never leave the visible window, whatever moves it. Typing in the filter resets the selection to the top row.

## Tabs

- **Installed** - one row per registry-backed plugin, plus its always-on token cost when `claude plugin details` has reported one (fetched once per installed version, then cached). The marketplace catalog renders after the installed rows - available plugins, dim, with an `Install` action - and the Available toggle hides it. Enter on an available row opens the install scope picker for its plugin. A registered plugin renders here even when its own directory ships no components (the LSP-only plugins carry nothing but the server their marketplace manifest declares). A row that cannot load still renders, with its failure reason: an install whose directory is gone, unreadable, not a directory, or absent from the registry entry, or a marketplace whose manifest cannot be read or cannot be parsed.
- **Skills / Agents / Commands** - one row per INSTALLED component of that kind, sourced by its plugin. The available stream's components of the same kind render after the installed rows by default, dim, with an `Install` action; the Available toggle hides them.
- **Hooks** - one row per installed plugin hook set with its trigger events. Available hook sets render after them the same way.
- **LSP** - one row per server the installed plugins' manifests declare, with the binary check. Available servers the marketplaces declare render after them the same way.
- **MCPs** - one row per live session server over the shared grammar: the state glyph from the connection status, the server name, the scope as source (`user`, `project`, `plugin`), the status in the status column (`connected`, `needs auth`, `pending`, `disabled`, `failed`), the transport as a bracketed badge, and the dim detail carrying the tool count and the command or URL. There is no summary band: the rows carry the state and the tab chip carries the count. Enter opens the per-server details overlay - status, enabled, scope, transport, tools, the server's configuration, and the Refresh / Reconnect / Disable actions - and Esc closes it.
- **Marketplaces** - one row per configured marketplace over the shared grammar: the health as the state glyph (`✓` healthy, `⚠` drift, `✗` a manifest that cannot load, `-` a scan that has not landed yet), the source kind as source (`github`, `directory`), and `healthy - N plugins` in the status column - or the drift notice (`registry drift - installLocation outside the config dir`), the failure reason, or a dim `scan pending` - with the repo as dim detail and `Repair` right-aligned on the drift and load-failure rows. The add row closes the list.

## The Available toggle

The action row carries `Available (a) +N` on every row-backed tab - the Installed tab and the component tabs - where `+N` is the available stream's row count for that tab. The available stream renders by default, after the installed rows: dim names, blue statuses, a plain `Install` action - visually secondary, exactly like the marketplace rows in the catalog. Pressing <kbd>a</kbd> hides it, and pressing <kbd>a</kbd> again shows it. MCPs and Marketplaces carry no toggle: those tabs have no available stream to hide.

<details>
<summary>Actions, update-all, and the update report</summary>

- <kbd>Enter</kbd> opens the selected row's overlay: a plugin's actions (Enable / Disable / Update / Roll back to previous version / Install in current project / Uninstall), the install scope picker for an available row - the catalog plugin itself on Installed, its source plugin on the component tabs - or the server details overlay on MCPs. Uninstall always confirms first - the CLI cannot remove one component alone, so the confirm names the whole bundle and its component count ("Removes superpowers and its 14 skills.").
- <kbd>a</kbd> toggles the Available stream on every row-backed tab; <kbd>u</kbd> updates exactly the stale installed rows - the count in `Update all (u) (N)` on the action row - one `claude plugin update` per entry, per-project cwd for project and local scopes, continuing past failures; <kbd>c</kbd> checks for updates and applies nothing; <kbd>r</kbd> refreshes the inventory.
- The inventory scan reads the plugin registry, the plugin cache, the marketplace manifests, the marketplace registry (`known_marketplaces.json`) and `settings.json` straight off disk. It runs when the page opens and on session change (the pane's five-second TTL), never spawning `claude` - the manual <kbd>r</kbd> and post-action refreshes keep the full CLI path - and first paint renders from the cached snapshot: the pane shows its loading copy while the first scan flies rather than blocking on a CLI spawn.
- The update report renders as it did on the old Plugins view: a bold header plus one row per plugin naming `plugin@marketplace` and its outcome, at most 10 rows before a "...and N more" line, cleared with <kbd>Esc</kbd> once finished. The global top spinner is never driven by extension actions.
- With `[plugins] auto_update = true` the update run fires once at forge boot. Each applied update (previous version, marketplace ref, when, trigger) is recorded in the machine-local store, so the overlay grows "Roll back to previous version" for a plugin whose record carries a marketplace ref, restored via the ref and verified against the refreshed inventory.

</details>

<details>
<summary>Marketplaces and add</summary>

The Marketplaces tab's action overlay carries Update, Remove, and - when the scan flags the marketplace as drifted or failed - Repair, which runs the CLI's remove-and-re-add pair and re-scans. Add marketplace keeps its own overlay, its input field wearing the unified single-line composer chrome: the draft (or the dim "`owner/repo or URL`" placeholder) inside a one-row rust-orange thick border, cursor block at the caret, and a live dictate take blips inside the border - the first <kbd>Esc</kbd> abandons the take, the next closes the overlay, and dictated words land in the field.

</details>
