# Extensions

The page that shows what this forge can run: the plugins installed here, the
skills, agents, commands, hooks and language servers they ship, the plugins a
configured marketplace offers that are not installed, and the marketplaces
themselves. It is drawn from one snapshot of the `extensions` subject - the
inventory, the PATH checks and the last update run - and redrawn from the
update stream after it. The drawing it is held against is
[web-extensions.html](./web-extensions.html), beside this page.

It lives at `/extensions`, which is its own address rather than a session's:
plugins belong to the machine's config dir, not to a seat, and the subject
carries no slot. The header's way back is the home.

The read and the actions are the server's (`Subject::Extensions`,
`Command::PluginInstall`, `Command::PluginUpdate`, `Command::PluginUpdateAll`,
`Command::PluginCheckUpdates`, `Command::MarketplaceAdd`,
`Command::MarketplaceRepair`). This page is the view of the inventory, and
the action verbs live in the core so that any view reaches the same CLI
rather than driving it from its own layer.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Header | the brand mark, `forge`, the page's name, and the way back to the home | `ClientSettings.mark` from the greeting; the route |
| Legend | the three marks in words, once: a triangle is an update, a spinner is work in flight, an X is a failure, and no mark is installed and current | the page's own constant |
| The page's state | a refused action in the core's own words, and an inventory that could not be read at all | `refusal` (an `error` frame); `snapshot: null` |
| Rail | one item per kind - Installed, Skills, Agents, Commands, Hooks, LSP - then Marketplaces under a hairline, each with its count; the Installed item carries a triangle when anything is stale | `snapshot.components`, counted over INSTALLED rows only |
| Installed | one row per installed plugin: a fold chevron, its state mark, its name, its marketplace, its version (and the marketplace's when they differ), its counts, and its own actions; the open row shows every component it ships, grouped by kind | `snapshot.components`; `update_run` for a row in flight |
| A kind's pane | one flat row per component of that kind: the name, the plugin it comes from, and (for LSP) the binary check; a row whose plugin has an update carries the Update action | `snapshot.components`; `lsp` |
| Updates band | how many plugins are behind the marketplace with their version pairs and Update all; a run in flight with its progress; a finished run's report | `update_run`, `snapshot` |
| From the marketplaces | one dim row per plugin a marketplace offers and this machine does not have, with Install | `snapshot.components` (not installed); `snapshot.marketplace` for descriptions |
| Marketplaces | one row per configured marketplace: its source, its repo or clone, its plugin count, and its drift or load failure with Repair | `snapshot.marketplaces`, `snapshot.marketplace_health` |
| Install scope picker | the plugin, the three scopes (`user`, `project`, `local`) and a project for the last two | the roster (the home's project list) |

## The rules

**Three marks, and words for everything else.** Ved, 2026-10-10: a
vocabulary of seven shapes is decode-work, not information. A triangle is an
update waiting, a spinner is work in flight, an X is a failure whose reason
is on the row, and **no mark** is the ordinary case. `auto-installed`,
`restart required` and `disabled` are chips in words, and the legend line
says the marks once so a shape is never a guess.

**A plugin row folds its components, one at a time.** The whole row is the
toggle (the hit target is an overlay behind the content, so the row's own
controls stay pressable), opening one closes the last, and the fold shows
**every** component the plugin ships, grouped by kind with counts: skills
and agents and commands by name, the hook set with its trigger events, and
each LSP server with its own check.

**Updates are per plugin, and all at once.** Every stale row carries its own
Update - one `claude plugin update` call for that plugin in that scope - and
Update all runs exactly the stale set, one call at a time, continuing past a
failure and reporting each row's outcome. A component is never updated
alone: install, update and uninstall act on the plugin bundle, so a
component row inside a fold carries no control of its own and the head row's
Update is the whole of it.

**The rail counts installed rows only.** A tab's count describes what is
here, never the catalog's size: 19 plugins with 74 installed skills shows
`Skills 74` even where the marketplaces offer more.

**Available rows name the plugin, not a component.** The install unit is the
plugin, so the marketplaces section lists plugins (with what they ship),
and the press opens the scope picker rather than installing silently.

**An inventory that cannot be read is its own state.** No plugins root
resolving (`CLAUDE_CONFIG_DIR` or `HOME`) is not an empty machine: the read
answers `snapshot: null` and the page says so rather than drawing an empty
tree.

**The PATH check is the server's.** Whether `rust-analyzer` resolves is a
fact about the machine the socket is on, so `lsp[].on_path` crosses computed
and the page never guesses: a check the server did not report draws no note.

**The band is the page's update state, not the query's.** Filtering the
search narrows rows; the count of stale plugins and Update all stay about
the whole read, because "there is something to do" must not disappear
because of a query.

## The states the page can be in

- **The ordinary case**: a full inventory, nothing stale, no band.
- **An update waiting**: the band names the pairs, the Installed rail item
  wears a triangle, and every stale row carries its own Update.
- **A run in flight**: the band carries the spinner and `n of m done`, and
  each row the run is on says `updating...`. The control a press started
  waits for it, and a second press while one runs is refused by name above
  the page - the core serializes plugin actions, and saying so is better
  than a control that silently does nothing.
- **A finished run**: the band's own report - `3 plugins updated`, or
  `1 updated, 1 failed` - with each failed row carrying its reason.
- **A check in flight**: the Check button says `checking...` and the read's
  next landing clears it; a check applies nothing.
- **A plugin whose directory is gone**: the row draws with an X and its
  reason, and its fold says so too.
- **A disabled plugin**: a chip in words; it is never in the stale set.
- **An auto-installed dependency**: a dim chip; an update waiting still
  outranks it with the triangle and the Update action.
- **A plugin mid-relaunch**: a hook whose update applied but has not been
  consumed draws `restart required` on its row. The reload is a session's
  own - the chip says so and the page does not offer to do it.
- **A marketplace that drifted**: the row carries the notice and Repair,
  which is the CLI's remove-and-re-add pair followed by a re-scan.
- **A marketplace whose manifest will not load**: the failure's own words on
  the row, and Repair beside it.
- **The scope picker open**: three scopes, a project select for the two that
  need one, and a note saying the press installs the whole bundle. The
  confirm press sends - focus lands on the first radio, and the button is
  the control - and Esc, the backdrop and Cancel close it; the hardware
  Back closes what it opened.
- **A refused action**: the core's own words above the page, straight off
  the dispatch - a second action while one runs, a project-scoped install
  with no project named, a project `forge.toml` does not declare.
- **An action that ran and failed**: the core's own words above the page -
  a failed update, install, marketplace add or repair, a plugin the registry
  does not hold in that scope, or a scan with no root to walk, all of which
  ride the `Plugins` frames rather than a reply - with the row's own reason on it
  where the run reported one, and a rejected add reopening its input with
  the draft kept, because a source that vanishes with its error cannot be
  fixed.
- **A search that matches nothing**: the lists empty and the counts under
  them say how many rows were on the page before the query.
- **Reading**: connected, and the subject's snapshot has not landed.
- **Refused**: the server turned the subscription down, in its own words.
  The address is not what is wrong, so the door is not drawn.
- **A connection that dropped**: the shell's own line stands above every
  page, and this one keeps what it last read.

## The pieces it takes from the pages beside it

- The header, the brand lockup and the `.wrap` column, from the home.
- The band card's ground - the gradient over `--line` - as every row's
  surface, and the chip, at this page's scale, from the dictation panel's.
- The three state marks are the shared vocabulary (`web.css`): the triangle
  is `.dot.warn`, the spinner is the ring, the X is `.dot.failed`, and the
  quiet case draws nothing at all.
- The search field, the band's shape and the section headings are the models
  page's own treatment, and the install dialog is the board's takeover shape
  at one question's size.

## The two widths

The page is checked at **1600** and at **430**, against the drawing beside
it. At the sheet's 760 breakpoint the rail wraps above the pane as a row of
chips, a plugin row's provenance wraps under its name, and the fold's
component grid becomes one column. Nothing overflows horizontally at either
width - the sheet has no sideways scroller anywhere.

## Touch

Every control takes the 44px target under `@media (pointer: coarse)` - the
shared rule covers a button and an input, and the page adds the five it
cannot reach: a plugin row, a rail item, a marketplace row, the install
dialog's scope row and the way back to the home. The fold's chevron is drawn
at rest rather than uncovered by the pointer, because the fold is the page's
one interaction and a phone has no pointer to arrive with. No affordance
lives on hover alone.
