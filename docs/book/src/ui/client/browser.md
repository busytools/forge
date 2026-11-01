# The browser surface

The server's Chromium, as a full-window takeover under a slim client bar.
A browser hand-off's **Open** and the strip's browser row both open it; the
sessions drive the same tabs while it is closed, and opening one shows the
tab as they left it. The drawing it is held against is
[web-browser.html](./web-browser.html), beside this page.

**The browser is the server's.** The server ships its own CEF host - a signed
`Forge Browser.app` beside forge's machine-local state, not in the client's
bundle - and runs one windowless host process per org profile, with a
vendored Playwright driver speaking MCP to that host's DevTools endpoint.
**A profile comes from `forge.toml`**: the org's `browser_profile` (default
`default`) with the project's own override, so a session's calls drive
`<org>/<name>` and no client names or picks one. The host owns the profile's
tabs and encodes each tab's painted frame as a WebP still onto a local
socket; the server relays the newest one to every viewer of that tab. The
client carries no engine, no driver and no browser profile: it is a viewer.

**The phone and the web are viewers too.** The same frames, the same bar, the
same Take control: the page paints each frame on a canvas where the desktop's
shell lays a native view. **The canvas keeps the picture's shape**: a wide page
in a portrait phone is scaled to fit with the takeover's own background above
and below it, and taking control lays the page out at the phone's shape, so
the bands fall away. **A finger drives the page the way a mouse does**: a
tap is a press and its release, a one-finger swipe is a wheel, and a tap on
the page raises the phone's keyboard - its named keys, inserted text and IME
compositions cross as the same key and text events the desktop overlay
sends. **The bar wraps on a finger's screen**: a hand-off's row is wider than
a portrait phone, so its actions take a line of their own and **Done**,
**something's wrong…** and **Not now** stay reachable at 393 and 430 points.
**The web page is the same client with no shell at all**: its takeover is a
fixed overlay the page owns, its canvas is the picture, and every call the
desktop makes to the shell - attach, list, detach, control, navigate, resize -
is the socket's own message instead. No client has a WebView engine or an
in-process node any more.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| The hand-off's card | `browser hand-off · tab N`; **Open browser**, **Done** and **Not now** before the takeover; while it is up, the card says the browser is open over the window and leaves the actions to the bar. **Words a lowered takeover handed to the chat are named here**, since the composer is behind this card until the ask is answered | the `browser_hand_off` ask, and the page's own surface state |
| The takeover's bar | **back to chat**, the page's **back** and **forward** arrows, the tab picker, the title (`the browser · <org>/<name>`, or `browser hand-off · tab N`), **take control** / **release control**, **Done**, **something's wrong…** and **Not now**; **try again** while the surface is in its failed state | the page's surface state, the parked hand-off, and the pushed `browser_view` and `browser_tabs` frames |
| The ask on the bar | this seat's parked hand-off: its own words under the bar row, with the tab its title names. Shown whenever the page is this seat's; **Done, Not now and the feedback line's send are offered only while that page is up on the tab the hand-off named** (live, or failed with its host gone), and **a page raised for another seat is lowered when that seat is left** | the `browser_hand_off` ask, and the page's own surface state |
| The feedback line | a text field and the dock's own **mic** and **send**: the person's words go to the agent as a prompt and the hand-off ends as *feedback* - they spoke, so nothing is recorded as a decline. A take begun at the bar's mic, or by the push-to-talk key wherever the caret is while the line's own toggle is offered, lands in this line, and a take already running ends on that same key. **The line holds those words in every phase the takeover stands** - opening, failed, or showing another tab - and showing them is not answering, so only the send waits for the page. **Lowering the takeover hands them to the chat's draft**, and the hand-off's card says so; **a send does not** - the words left for the agent, so the line clears and the draft gets nothing. **A take replaces the field with the dock's own card**, and the card takes the line: it grows to everything the mic leaves, so its graph is the row's meter rather than a pill in an empty row. **A slow open says so in a line above the row**: one brief enough to be the press itself draws no line, and a longer one says the microphone is opening. **A send the connection never took says so in that same line**, in the refusal tone - `Not sent - the connection is down.` - and the words stay in the field: the bar does not clear what did not leave, and the hand-off stays parked for the retry. The card draws from this page's own take, which exists only once the capture has opened, and the composer's own band is behind the picture | the page's own state, and the take it is opening; the prompt rides the socket |
| The surface | the viewed tab's page, filling the window under the bar: no window, no browser tab strip and no browser chrome | the server's relayed frames for that tab - drawn by the desktop's native overlay, or by the phone's and the web page's canvas |
| The strip's row | `browser` with its tab count, a dot when the host is connected (an idle dot when only the host is up, a cross when nothing runs), and a hover naming the profile, the connection and `closing in Nm`; its panel lists the tabs with **show** / **hide**. **The row is drawn only while the profile has a tab**: a browser nothing has invoked is no row at all | the `browser_tabs` ask and its pushed frame |
| A tab row | the host's number, the title (else its URL), the owner as `org/project/label`, and its state - **in use**, **idle** or **held** - with the mark that says which | the tab list's rows |
| The engine line | the panel's own line when no close is scheduled: `the server's own Chromium`, on every client, or `nothing to run a page` | the tab list |
| The states | `opening the browser…`, the server's sentence when it failed, the host's own death sentence - `the browser host exited (signal 9; pid 1234); the next call brings it back` - when a host dies under the surface (the cause is a signal or an exit code), and the page's own error page when a load fails | the page's surface state, and the server's failure frame |
| A refusal | the server's own words: `tab N is not open`, the profile has no tab to show yet, the host is not answering, or a page area that could not be placed. **A take the bar's mic pressed and could not open says why above the feedback row**: the composer's own notice row is behind the picture while the takeover is up | the attach's answer, the surface's failure frame, and the take's own line |

## The rules

**One host per org profile, and the host outlives the surface.** A profile's
browser is a server process of its own, started on its first real need - the
first seat spawned under the profile, a session call, a hand-off's Open, the
strip's show. Closing the surface ends nothing: the sessions keep driving the same
tabs, and the next Open shows one as it stands. An idle close ends the host
and its tabs and keeps the profile's cache and logins; the next call starts a
fresh host. Every page call logs its seat, tool, profile, host pid, port and cache
root, so a call's browser is answerable after the fact.

**A profile is `forge.toml`'s, not a seat's.** Its key is `<org>/<name>`: the
org's `browser_profile` (default `default`), or the project's own override.
A session's calls drive that profile and land on its own tabs. **There is no
profile verb**: no client names, closes, deletes or creates one, and the
segment's list is tabs rather than profiles.

**The host owns the tabs, and a tab belongs to the session that drives it.**
A tab carries the host's number, and the registry keeps its owner, its title
and URL, and the session's current tab. A session's first page call creates
its tab - or adopts the host's own boot tab - and every later call drives
that tab; another session's calls never land on it. **A page's `window.open` opens a
tab of its own**, owned by the opener's session and made that session's current
tab, so a sign-in popup is driven like any other page; a popup that closes
itself hands the session back to the tab that opened it. Three things close a tab
with its session: the sweep of dead seats, a `/new` or `/resume` on the slot,
and the host ending - except a tab a parked hand-off still holds. Nothing
outlives the host.

**A popup's own document is the one the host starts.** Its target answers no
page-domain command until it has a document, so the passkey veto - the script
every other page starts with - cannot be registered on it before that first
one, and a popup an attached client holds commits exactly one navigation, so
its address cannot be preceded by a blank. The host starts the address the
popup asked for as that first document; the veto patches that document's world
as soon as its context is reported, which is the out-of-band case a tab
another client creates directly at a URL is already in.

**The browser offers no WebAuthn**: its pages expose no `PublicKeyCredential`,
so passkey and security-key sign-in stop working and a site that checks for the
API asks for a password instead.

**The old profile trees go once.** The first server that finds no marker
deletes the client's old in-process CEF caches - `browser/cef-profile` and
`browser/profiles/<name>/cef` under its bundle identifier - and the server's
own pre-org `shared/` and per-seat `profiles/<name>/cef` trees, then writes
`legacy-profiles-deleted` beside the root; a later boot does nothing. Org
profiles live one level deeper, at `profiles/<org>/<name>/cef`, and a stray
file under `profiles/` is not a profile.

**A hand-off names a tab.** `browser_hand_off` requires the page id of one of
the session's own tabs, as `browser_tabs` prints it, and **Open** raises that
tab. A page id no tab carries is refused with the tabs as they stand. The tab
is held while the ask is parked: the idle clock and the dead-seat sweep both
leave it alone, the strip marks it **held**, and the hand-off's card names
it. Any answer releases it - Done, Not now or feedback - and a session that
went while the tab was held takes it with it when the ask resolves.

**A page context that dies is rebuilt and answered in the client's words**,
never handed back as the driver's `_page` TypeError. A driver that cannot
attach within its 30 s bound is replaced; a host no fresh driver can list is
named and ended, except that a host another session's page is open on is left
running and the holder is named back instead; when its endpoint has stopped
answering, only the idle clock releases it.

**Every viewer is a watcher until it takes the role on that tab.** A surface
attaches to one tab and receives every frame that tab paints; **exactly one
attached viewer holds the controller role on it**, and only its gestures and
history arrows reach the page. **The layout follows the role, and with nobody
holding it it belongs to the tab's longest-watching viewer** - so a page with
no shell still lays its own view out while it is the only one watching, and a
second watcher never moves the page under the first. The bar's **take
control** moves the role to this surface (the previous holder becomes a
watcher at that moment), and **release control** gives it back with nobody
holding it. A watcher's
arrows are drawn disabled: the move is the controller's, and the server
refuses one from anyone else. The server pushes who holds it to every viewer
of the tab, so two surfaces draw the same truth, and each knows its own viewer
id - the bar reads whose role it is rather than guessing. **The bar also names
the browser it is over** - the profile's own key - so two devices driving two
profiles cannot read as one browser with a role held twice. **The bar's picker
moves this surface to another tab of the same profile**: a re-attach on that
tab, which replaces what this connection was watching.

**The picture is the server's own frame.** The host paints windowless and
encodes a WebP still; the server relays the newest frame for the viewed tab
to each viewer, and the client decodes it - into a layer at the page area's
rect on the desktop, onto a canvas on the phone and on the web. **Every client
keeps the picture's shape**: a frame laid out for another viewer's surface is
fitted with bars rather than stretched, and taking control (or, on the web,
being the tab's longest-watching watcher while nobody drives it) lays the page
out at this surface's shape so the bars fall away. There is no screen capture anywhere, so the
Screen Recording grant, the picker and its monthly re-approval are not part
of this surface.

**A page with no shell draws its own takeover.** On the web the overlay is a
fixed box the page owns - the bar at the top, the canvas under it - and every
call the desktop makes to its shell is a socket message instead: an attach
names the seat and tab and its reply says which profile and tab it took, the
tab list's ask subscribes the push that follows, a lower is a detach, the role
is a control message, the arrows are a navigation, and the surface's size is a
resize. There is no rect to report, because the page's own box is the surface;
the canvas still draws only the viewed tab, and a key or a click rides the same
input messages the desktop and the phone send.

**The row exists only while there is something behind it.** A profile with no
tab is a browser nothing has invoked: the segment draws nothing at all rather
than an empty row saying `0 tabs`, and the list's own pushes bring it in with
the first tab and take it away when the last one closes.

**Show and hide are this client's own.** The marks come from the server's tab
list - the row's dot means the profile's host is connected (an idle dot means
the host is up with no driver), and a tab's own mark says in use, idle or held
- but each row's button reads this page's own surface: a phone or a web page
shows **show** while the desktop holds the picture on that tab, and lowering
here gives only this client's viewer back.

**A windowless page must stay a visible page.** The host asks CEF for a
visible, focused page (`WasHidden(0)`, `SetFocus(1)`) and drives its frames
from an explicit begin-frame cadence of one every 8 ms - CEF's own scheduling
under an external message pump painted the same page at about half the
screen-refresh rate. The
sessions' tools - navigate, snapshot, click, type, evaluate - are the same
vendored Playwright battery over the same CDP connection; nothing on the
wire changed.

**A snapshot's refs belong to the snapshot that minted them.** A ref names
an element that was visible and pointer-interactive when the snapshot ran,
and it survives an ordinary re-render that keeps that DOM node. A page that
keeps replacing its elements retires refs before the next call, so it is
driven by a unique role/name selector such as `role=button[name="Save"]` -
scoped to a row when names repeat - which is resolved when the call runs; a
name shared by several elements is refused rather than guessed at. Once the
page has settled, a full-page `browser_snapshot` with no `target` and its
refs hold: it keeps the ref of an element that stayed and carries the same
role and accessible name, gives none to one that is no longer visible or
pointer-interactive, and mints a new one when the role or name changed. A
target-scoped snapshot answers with that subtree alone, so refs outside it do
not resolve until a full-page snapshot.

**Input is trusted input.** The person's mouse, wheel and keys are mapped by
the SERVER to CDP `Input.*` events at the page's own scale: a click is a
full pointer sequence, a wheel is a pixel delta, printable text is inserted
as text, the specials (Enter, Tab, the arrows, Escape) are key events, and
Cmd+V reads the pasteboard as text. The phone's canvas sends the same
gestures from its own pointer events. A gesture's point is scaled from the
surface's layout into the page's viewport, which need not be the same size.
Widgets that gate on trusted input commit, which is the whole reason the
events are replayed rather than synthesised in the page. **The desktop
overlay claims the menu's chords while it has the keyboard.** AppKit offers
key equivalents before the page's keys, and Tauri's default menu takes
Cmd+A, X, C, V and Z (and Close Window on Cmd+W) even with the surface
focused. So while the overlay holds first responder, those five go to the
page - Cmd+V as the pasteboard's text - Cmd+W lowers the takeover the way
back to chat does, and Cmd+[ and Cmd+] move through the page's history. With
the bar's own feedback field focused, the menu keeps its editing as it does
everywhere else, and every other Command chord belongs to the app.

**The arrows are the page's own history.** The server reads
`Page.getNavigationHistory` over the same CDP connection that carries the
input - on attach, on every re-open, and after every navigation the page
commits, including a same-document one - and pushes the two booleans to the
bar. An arrow is disabled exactly when the history has no entry that way;
back and forward call `Page.navigateToHistoryEntry` with the neighbouring
entry's id. A query that cannot be read is no news: the arrows stay exactly
as they were rather than guessing a new state. There is no URL bar: the
history is the page's own, and the arrows are its only map.

**A page's file chooser is the controlling client's picker.** The host has
no window of its own, so the page's chooser - captured by the driver's
Playwright when one is attached, or CEF's own dialog when none is - goes to
the controlling client as a `browser_file_dialog`, and that shell opens the
platform's own picker: the desktop's panel, the phone's file browser. Only
the page's current tab is asked; a chooser on a background tab is left alone,
because the upload answers the current tab. The chosen files' bytes travel
back to the server, which writes them beside the profile and hands them to
the captured chooser. A dismissed picker cancels it, so a page never blocks
on a panel nobody can see; **only the controller is asked**, and a watcher is
not cancelled by anyone - it simply ignores the ask. **Past sixteen files or
8 MiB the whole answer is a dismiss**, and the cap is 8 rather than 32
because the answer crosses as base64 in one socket message: the socket's
inbound bound is 16 MiB, so 8 MiB of bytes is what really arrives instead of
a promise the connection tears on. On the desktop, Cmd+A, X, C and Z send the page's editing commands
and Cmd+V is a text paste. The phone's keys, inserted text and IME
compositions reach the page through the same input path (above); a rich
clipboard and context menus are not part of this cut.

**A page's download is saved by the controlling client.** The host writes
the file beside the profile and announces it as a `browser_download`; the
shell opens the platform's own save door and writes the bytes there, or says
why it could not. A file past 32 MiB is refused in words rather than handed
over, and **only the controller of that tab is sent one**. **A drop the
backlog lost names the profile rather than a tab**: that notice carries no
tab, says how many finished downloads were dropped and that the profile's
backlog holds at most four at a time, and its title reads one loss or many
from the count.

**The page's sound plays at the controlling client, for the viewed tab.** The
host relays what the page plays as PCM packets beside the frames, each
carrying its tab; the shell plays the tab it is showing and drops another
tab's packet, on this machine - the desktop's audio engine, the phone's web
audio. A watcher is looking at the page rather than driving it, so they hear
nothing.

**A recording is the agent's, and the person sees its layout.** While an
agent records the page, the page is laid out at the file's own 1280x720 and
put back when the take ends, so the file is the layout the page had rather
than a scaled copy of the surface.

**There is no external-browser door.** Every page lives in the server's
engine and nowhere else; no client hands one to another browser.

**A host that ends takes its tabs.** A restart, an idle close and a stop all
end the host, and its tabs end with it: the next call starts a fresh host,
whose boot tab the first page call adopts before navigating. A profile's
cache and logins stay, so a sign-in survives the host; a filled form does not.

**An idle browser closes on the SERVER's clock.** After `[browser]
idle_minutes` (30 unless the config says otherwise) with no call and no viewer
attached, the profile's host is closed - there is no warning and no second
grace clock. A viewer attached and a tab a parked hand-off holds keep the
profile open: the sweep skips both rather than closing a page someone is
looking at. **A lower does not reset the clock** - the next call or attach
does. The close ends the host and its tabs, not the profile's cache or
logins.

**The catalogued engine's limits are named, not hidden.** The prebuilt CEF
has no H.264 decoder (the job sites the probe exercised do not need it), it
presents plain Chromium's user agent and brands (a real browser's identity,
not an automation tell), and its frames are software-rendered (measured at 60
frames per second at the surface's size; CPU was not profiled).

**A failure is drawn where the person is looking.** The takeover's page area
carries the server's sentence and the hand-off stays parked; the strip says
what a refused show or hide said. **Done**, **Not now** and **feedback** are
the hand-off's answers; the feedback line sends its words as a prompt and,
once they leave, ends the park as **feedback**, saying the person spoke
rather than that they declined. **A send that did not leave says so in the
bar's own line and ends nothing**: the words stay in the field, and the park
waits for the retry.

**An answer belongs to the page it answers.** The takeover is one seat's page:
leaving that seat lowers it, so it never sits over a seat whose ask is waiting.
The bar carries the ask while the page is the asking seat's, and offers Done,
Not now and the feedback line only while that page is up on the tab the
hand-off named; a page moved to another tab names the ask and answers nothing.
**A failed page stays answerable**: a host that died under a parked ask is what
the feedback line is for.

## The widths

The takeover is the whole window, under a bar that is one row at a mouse's
width. **A narrow window or a finger's screen wraps the actions onto a line of
their own** - a hand-off's row is wider than a 393-point phone - so **Done**,
**something's wrong…** and **Not now** keep their 44px targets and stay on
screen; the page area is the rest.

**The takeover is a layer in the window the person already has**, so the
title bar and its traffic lights stay where they are and leaving is the
ordinary window. Back to chat, the strip's hide and the phone's hardware Back
all lower the layer the same way, whatever opened it, and the strip and the
dock return with it. **On the web it is a layer over the tab's page**, fixed to
the viewport, so the chat is still under it and Back to chat returns to it
without leaving the app.
