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

A save is one `settings_edit` message: the fingerprint, and the batch of
edits (`set`, `add` or `remove`). The server reads the file, compares the
fingerprint, applies the edits to its text with a comment-preserving
writer, validates the candidate with the boot loader, and renames a temp
file over the path. The watcher then reloads and its settings push is the
save's answer; a save that wrote nothing is answered by `settings_refused`
with the loader's own words. A push that never arrives is a timeout the
bar reports.

## What it draws

| Piece | What it is |
| --- | --- |
| Header | The mark, `settings`, the server / socket / claude versions, the mode chip (`edit` or `editing`), and the totals (keys, areas, the resolved `forge.toml` path). |
| Legend | The four effect classes, each as a shape and its word. |
| Sub-nav | The twelve areas, each with its key count and every class its keys hold. |
| Pane | The area's name and note, then its rows grouped by the instance their path names (an account, a model, an org and a project, a Slack workspace, the table a server key belongs to). |
| A row | The key's leaf name, its value, and its class mark; the whole path is the row's title, and the group header carries the instance. |
| The editor | The `edit` chip in the header turns every value into its control, adds the add doors and per-entry removals, and draws the save bar. |
| Save bar | The unsaved count, Save and Discard, and the refusal in the loader's words when a save writes nothing. |
| Restart banner | After a save lands, exactly the restart-class keys it carried, with no restart button. |

## The rules

- **One catalogue.** The class of every key comes from the config loader's
  catalogue (`crates/forge-workspace/src/settings.rs`), which is also what
  the reload summary reports from. The page cannot label a key differently
  from the reload.
- **Rows are never skipped.** An area, a key, a class or a value shape this
  client does not know draws plainly with the server's own word; nothing
  disappears because a build is older than the document.
- **A row draws its leaf.** The area header names the table and the group
  header names the instance, so a row draws the key's own name, which the
  server sends as `leaf`; the whole path stays in the document and in the
  row's title. A name containing dots, like a model slug, is one name.
- **An area reports every class it holds.** A sub-nav row draws each
  distinct class its keys carry, not only the furthest, so an area holding
  both a restart key and an unread one says both.
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
- **Unsaved** - the bar counts the batch and Save is the one door.
- **Saving** - the button says so and the bar waits for the push.
- **Refused** - stale (a reload-page door), invalid (the loader's words,
  anchored at its key), or a guard's own refusal.
- **A save with no push** - the bar says no reload arrived and keeps the
  batch.
- **A new entry** - a skeleton in place with its required fields marked;
  an added entry is discarded or saved with the rest.
- **A pending removal** - the entry reads as removed on save, with Undo,
  until the save lands.

## The pieces it takes from the pages beside it

The shared header (`header.top.pagetop`), the brand lockup, the type scale,
the palette tokens, and the `.wrap` root. Its rows and rail are its own,
scoped under `.settings-page`, because `.row`, `.rail` and `.chip` are names
other surfaces already use; the class marks are the shared
`components/ClassMark.svelte` and their rules are not scoped, so a second
surface can wear them.

## The two widths

Checked at 1600 and 430. At 1600 the rail is a fixed 232px column and the
pane takes the rest. At 430 the rail wraps into a row of bordered chips
above the pane, the back link leads the header with the title on its own
line and no mode chip, and the table keeps its three columns with tighter
key text.

## Touch

The sub-nav items, the back link, the mode chip, every add and remove
action, the save bar's buttons, the toggle, the list controls and the
editor's boxes are 44px targets under `(pointer: coarse)`. Nothing on the
page is hover-only: the selected area carries a background, an inset bar
and `aria-pressed`, every class mark is a shape plus a word, and every
control has its own label.
