# Settings

The effective config: every key `forge.toml` loaded, its value, and when an
edit to it takes effect - and the editor that changes those values, adds
and removes whole entries, and saves the batch. The drawing is
[web-settings.html](./web-settings.html).

## Its address

`/settings`. It is a member of the client's routes, so a deep link opens it
and the desktop and web clients draw the same page.

## Where its data comes from

There is no settings read verb. The server pushes the whole document as a
`settings` frame once after the greeting on every connection, and again
after every applied reload; the page holds the last frame it was handed. A
client that attaches while a reload is in flight reads the current load
from the next push, and the attach frame always reflects the current one.

The document is built once per load and shared by every connection. Its
values cross unredacted, which is the decision the WireGuard boundary
carries. It also carries the fingerprint of the raw file text it was built
from, and the edit metadata for every key.

A save is one `settings_edit` message: the fingerprint, the batch of edits
(`set`, `add` or `remove`), and the page's own `id`. The server reads the
file, compares the fingerprint, applies the edits to its text with a
comment-preserving writer, validates the candidate with the boot loader,
and renames a temp file over the path. `settings_saved` acknowledges the
save by its `id`; one that wrote nothing is answered by `settings_refused`
with the loader's or the guard's own words. The watcher's reload push
carries the new document to every client. A save with no answer is a
timeout the bar reports.

## What it draws

| Piece | What it is |
| --- | --- |
| Header | The mark, `settings`, the server / socket / claude versions, the totals (keys, areas, the resolved `forge.toml` path), the back link and the edit button. |
| Sub-nav | The twelve areas, each with its key count and every class its keys hold. On desktop it stays in place while the pane scrolls. |
| Pane head | The area's name and note, its key count and, where the area names entries, how many (projects counts projects, not the orgs that carry them); the area's add doors in edit mode. It stays above the scroll. |
| A card | One entry or table (an account, a model, an org or a project, a Slack workspace, a server table): its name, key count and own doors, then one line per key. Keys with no instance draw one card with no head band. |
| A setting line | The key's leaf, its class badge, and the value right-aligned to the card's edge on desktop (stacked under the key at a phone width); the whole path is its title. In edit mode the value is the control, the required mark rides the key, and a changed line keeps an accent edge. |
| The editor | The edit button in the page header turns every value into its control, gives each entry card its remove door (an org card `+ project` instead), and draws the save bar. |
| Save bar | The unsaved count and one claim: a refusal, a conflict, saving, saved, or nothing. A conflict offers its reload even when a refusal outranks it in the claim, and a stale refusal offers one too; a refusal the loader named anchors at its key. |
| Restart banner | After a save lands, exactly the restart-class keys it carried, with no restart button. |
| A new entry | A dashed card headed by its name and `new <group>`, its fields empty and required-marked, discarded or saved with the batch. |
| A pending removal | The card head reads `removed on save`, with Undo; its lines are not drawn. |

## The rules

- **One catalogue.** The class of every key comes from the config loader's
  catalogue (`crates/forge-workspace/src/settings.rs`), which is also what
  the reload summary reports from. The page cannot label a key differently
  from the reload.
- **The class is stated once.** Each line carries its badge as a shape and a
  word; there is no page legend and no second copy in a card head. An area's
  rail row still reports every class its keys hold, which is a different
  fact: which classes live in an area.
- **Lines are never skipped.** An area, a key, a class or a value shape this
  client does not know draws plainly with the server's own word; nothing
  disappears because a build is older than the document.
- **A line draws its leaf.** The pane head names the area and the card head
  names the instance, so a line draws the key's own name, which the server
  sends as `leaf`; the whole path stays in the document and in the key's
  title. A name containing dots, like a model slug, is one name.
- **An area reports every class it holds.** A sub-nav row draws each
  distinct class its keys carry, not only the furthest, so an area holding
  both a restart key and an unread one says both.
- **The edit door is at every width.** It is a 44px bordered button in the
  page header, on the title row at a phone width, and no width rule hides
  it. It is disabled while a save is in flight.
- **The bar makes one claim.** Its status is derived in one order - refusal,
  conflict, saving, saved, idle - and drawn from one slot, so two can never
  appear together. A foreign push clears a saved flash before the conflict
  draws.
- **The client's axes come from this frame.** `[client] theme`, `font` and
  `mark` are applied when the document lands, so an open page re-themes,
  re-fonts and re-marks itself without a reconnect. The greeting still
  carries the same values for the pre-attach handshake, but only the frame
  applies them.
- **A frame belongs to its connection.** A submit can leave a launch's
  socket speaking after the person's connection took the page; the shell
  moves the store to the taken connection, so a stale document cannot
  re-theme the page someone is reading.
- **Batched saves.** Every changed value, added entry and removal is one
  message, one validated write, one reload and one push.
- **A stale save writes nothing.** The page carries the fingerprint it
  loaded; a file that moved since refuses the save and offers a page
  reload.
- **The loader is the authority.** A candidate that would not load writes
  nothing; the refusal is the loader's own message, anchored at the edit it
  names where it names one.
- **The writer preserves the file.** A value changes in place with its
  comments and its order; a new entry is appended; a removal takes only its
  own table.
- **Guards live on the server.** Removing a project with live sessions is
  refused, naming them, because the reload would leave those sessions
  running but invisible. Removing an account whose usage probe has not
  settled is refused until it does, because a retired entry still loading
  would leave the pool unsettled (#2681). Removing a settled account keeps
  its bound sessions on the retired credential until they end; removing a
  Slack workspace leaves its pump on the old token until a restart. The
  confirm says what each removal leaves behind.
- **No restart button.** A landed restart-class key is named in a banner;
  the server restart is the reader's.
- **Values are ordinary.** A token is drawn and edited as its value, like
  every other string; there is no redaction here, and the file remains the
  one source.

## The states the page can be in

- **Waiting** - connected, and the settings frame has not landed. The page
  says it is reading the effective config.
- **A failed read** - the frame arrived and this client could not read it.
  The page says so rather than drawing an empty config.
- **An unknown area** - a selected area the document does not hold (a
  document from a different forge build can leave one out). The pane says
  it is gone and offers the first area.
- **An empty area** - an area with no keys, which a section that is absent
  or off can draw.
- **No areas** - a document that declares nothing.
- **No connection** - the route draws the connect door, as the models page
  does.
- **Read-only** - cards, badges and values, with the edit button as the one
  door.
- **A long area scrolled** - the rail and the pane head stay in place while
  one area's cards pass under them.
- **Editing** - controls, required marks, the dirty line, and the add and
  remove doors.
- **Unsaved** - the bar counts the batch and Save is the one door.
- **Saving** - the bar says so, and it waits for the save's answer.
- **Saved** - the bar says saved for a moment, and the restart banner names
  the restart-class keys the save carried, if any.
- **Refused** - stale (a reload-page door), invalid (the loader's words,
  anchored at its key), a guard's own refusal, or `io` (the file could not
  be read or written; nothing was applied).
- **A conflict** - another writer pushed a different file while the draft
  was dirty; the draft is kept, saving is withheld, the saved flash is
  cleared, and the bar offers a reload.
- **A save with no answer** - the bar says no answer arrived and keeps the
  batch.
- **A new entry** - a dashed card with its required fields marked; it is
  discarded or saved with the rest.
- **A pending removal** - the card reads as removed on save, with Undo,
  until the save lands.

## The pieces it takes from the pages beside it

The shared header (`header.top.pagetop`), the brand lockup, the type scale,
the palette tokens, and the `.wrap` root. The rail, the cards and the
setting lines are its own, scoped under `.settings-page`, because `.row`,
`.rail` and `.chip` are names other surfaces already use; the class badge
is the shared `components/ClassMark.svelte` and its rules are not scoped,
so a second surface can wear it.

## The two widths

Checked at 1600 and 430. At 1600 the rail is a fixed 232px column that
stays in place, and the pane takes the rest. A setting line is 40px, or
48px while editing; its value is capped at 52%, and an editing control at
420px. At 430 the header is a grid: the back link and brand first, the
title and the edit button second, the build facts beneath. The rail wraps
into bordered chips, the pane is one column of cards,
a line stacks its key and badge over its value, and an editing control is
full width and 44px tall. The page does not scroll sideways at either
width.

## Touch

The sub-nav items, the back link, the edit button, every add and remove
action, the save bar's buttons, the toggle, the list controls and the
editor's boxes are 44px targets under `(pointer: coarse)`, and the edit
button is 44px at every width because it is the page's only way in. Nothing
on the page is hover-only: the selected area carries a background, an inset
bar and `aria-pressed`, every class mark is a shape plus a word, and every
control has its own label.
