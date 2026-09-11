# Extensions

`/extensions` opens the Extensions page: one full-frame view over everything installable - the plugin tier, each plugin's component kinds, the session's MCP servers and the configured marketplaces. It replaces the retired `/plugins` and `/mcp` commands and renders on the shared full-screen page scaffold - the outer titled box, full-width body and footer that [/usage](./usage.md) and [/diff](./diff.md) also render on.

## Tab bar

Eight flat sibling tabs, each with its live count: **Installed · Skills · Agents · Commands · Hooks · LSP · MCPs · Marketplaces**. The active tab is black-on-rust-orange bold, the rest white bold. There is no second navigation level: each component row carries its source plugin, which preserves the plugin-to-component tier in the row itself.

## Row grammar

Every tab but MCPs and Marketplaces renders one row per extension over the same grammar - state glyph, name, source, installed-vs-available status, badges, action. The status column is fixed-width and never wraps: `installed 6.3.0`, `6.3.0 -> 6.4.0 available`, `available 1.9.19 - not installed`, `disabled`, or `failed: <reason>`. A name too long for the row truncates with an ellipsis instead of pushing the status off the line.

- State glyphs: `✓` green for installed, `⚠` warning for an available update, `-` dim for available-not-installed, `✗` red for disabled or failed.
- Bracketed badges ride the row where they apply: `[auto-installed]` on a plugin the registry installed as someone's dependency, `[restart required]` on a plugin whose update applied but is not live until a restart consumes it.
- The row's primary action hangs at the end: `Update` on a stale row, `Install` on an available-not-installed row. The selected row wears a `>` marker; the full action set lives in the Enter overlay.
- Detail rides the row too: hook rows name their trigger events (`SessionStart, PreToolUse`), LSP rows state the binary check (`rust-analyzer: on PATH` / `gopls: missing`, resolved against `PATH` once per refresh).

## Tabs

- **Installed** - one row per plugin, plus its always-on token cost when `claude plugin details` has reported one (fetched once per installed version, then cached).
- **Skills / Agents / Commands** - one row per component of that kind, sourced by its plugin.
- **Hooks** - one row per plugin hook set with its trigger events.
- **LSP** - one row per server the marketplace manifests declare, with the binary check.
- **MCPs** - the MCP page's content unchanged: the status-badge summary line, then one row per server (name, status badge, scope badge, transport badge, dim summary), with the same details overlay and actions the standalone view had.
- **Marketplaces** - one row per configured marketplace: `healthy · N plugins` in green when the manifest loads, otherwise the drift notice (`registry drift - installLocation outside the config dir`) or the load failure reason, with the Repair action offered.

<details>
<summary>Actions, update-all, and the update report</summary>

- <kbd>Enter</kbd> opens the selected row's overlay: a plugin's actions (Enable / Disable / Update / Roll back to previous version / Install in current project / Uninstall), the install scope picker for an available plugin, or the server details overlay on MCPs. Uninstall always confirms first - the CLI cannot remove one component alone, so the confirm names the whole bundle and its component count ("Removes superpowers and its 14 skills.").
- <kbd>u</kbd> updates exactly the stale rows - the count in `Update all (u) (N)` on the action row - one `claude plugin update` per entry, per-project cwd for project and local scopes, continuing past failures; <kbd>c</kbd> checks for updates and applies nothing; <kbd>r</kbd> refreshes the inventory.
- The inventory scan reads the plugin cache, the plugin registry and the marketplace manifests straight off disk, refreshed on the pane's five-second TTL, and first paint renders from the cached snapshot - the pane shows its loading copy while the first scan flies rather than blocking on a CLI spawn.
- The update report renders as it did on the old Plugins view: a bold header plus one row per plugin naming `plugin@marketplace` and its outcome, at most 10 rows before a "...and N more" line, cleared with <kbd>Esc</kbd> once finished. The global top spinner is never driven by extension actions.
- With `[plugins] auto_update = true` the update run fires once at forge boot. Each applied update (previous version, marketplace ref, when, trigger) is recorded in the machine-local store, so the overlay grows "Roll back to previous version" for a plugin whose record carries a marketplace ref, restored via the ref and verified against the refreshed inventory.

</details>

<details>
<summary>Marketplaces and add</summary>

The Marketplaces tab's action overlay carries Update, Remove, and - when the scan flags the marketplace as drifted or failed - Repair, which runs the CLI's remove-and-re-add pair and re-scans. Add marketplace keeps its own overlay, its input field wearing the unified single-line composer chrome: the draft (or the dim "`owner/repo or URL`" placeholder) inside a one-row rust-orange thick border, cursor block at the caret, and a live dictate take blips inside the border - the first <kbd>Esc</kbd> abandons the take, the next closes the overlay, and dictated words land in the field.

</details>
