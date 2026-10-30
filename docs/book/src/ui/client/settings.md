# Settings

The read-only effective config: every key `forge.toml` loaded, its value,
and when an edit to it takes effect. The drawing is
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
carries.

## What it draws

| Piece | What it is |
| --- | --- |
| Header | The mark, `settings`, the server / socket / claude versions, the `read-only` chip, and the totals (keys, areas, the resolved `forge.toml` path). |
| Legend | The four effect classes, each as a shape and its word. |
| Sub-nav | The eleven areas, each with its key count and its strongest class. |
| Pane | The area's name and note, then its rows grouped by the instance their path names (an account, an org and a project, a Slack workspace). |
| A row | The key path, its value, and its class mark. |

## The rules

- **One catalogue.** The class of every key comes from the config loader's
  catalogue (`crates/forge-workspace/src/settings.rs`), which is also what
  the reload summary reports from. The page cannot label a key differently
  from the reload.
- **Rows are never skipped.** An area, a key, a class or a value shape this
  client does not know draws plainly with the server's own word; nothing
  disappears because a build is older than the document.
- **The client's axes come from this frame.** `[client] theme`, `font` and
  `mark` are applied when the document lands, so an open page re-themes,
  re-fonts and re-marks itself without a reconnect. The greeting still
  carries the same values for the pre-attach handshake, but only the frame
  applies them.
- **A frame belongs to its connection.** A submit can leave a launch's
  socket speaking after the person's connection took the page; the shell
  moves the store to the taken connection, so a stale document cannot
  re-theme the page someone is reading.
- **Read-only.** Nothing on this page writes the file. Editing is the next
  cut.
- **Values are ordinary.** A token is drawn as its value, like every other
  string; there is no redaction here, and the file remains the one source.

## The states the page can be in

- **Waiting** - connected, and the settings frame has not landed. The page
  says it is reading the effective config.
- **A failed read** - the frame arrived and this client could not read it.
  The page says so rather than drawing an empty config.
- **An unknown area** - a selected area the document no longer holds (a
  reload can remove one). The pane says it is gone and offers the first
  area.
- **An empty area** - an area with no keys, which a section that is absent
  or off can draw.
- **No areas** - a document that declares nothing.
- **No connection** - the route draws the connect door, as the models page
  does.

## The pieces it takes from the pages beside it

The shared header (`header.top.pagetop`), the brand lockup, the type scale,
the palette tokens, and the `.wrap` root. Its rows, rail and class marks are
its own, scoped under `.settings-page`, because `.row`, `.rail` and `.chip`
are names other surfaces already use.

## The two widths

Checked at 1600 and 430. At 1600 the rail is a fixed 232px column and the
pane takes the rest. At 430 the rail wraps into a row of chips above the
pane, the header's meta takes its own line, and the table keeps its three
columns with tighter key text.

## Touch

The sub-nav items, the back link and the unknown-area's link are 44px
targets under `(pointer: coarse)`. Nothing on the page is hover-only: the
selected area carries a background, an inset bar and `aria-current`, and
every class mark is a shape plus a word.
