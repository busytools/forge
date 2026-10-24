# The browser surface

The server's Chromium, as a full-window takeover under a slim client bar.
A browser hand-off's **Open** and the strip's browser row both open it; the
sessions drive the same page while it is closed, and opening it shows exactly
what they left. The drawing it is held against is
[web-browser.html](./web-browser.html), beside this page.

**The browser is the server's.** The server ships its own CEF host - a signed
`Forge Browser.app` beside forge's machine-local state, not in the client's
bundle - and runs one windowless host process per profile, with a vendored
Playwright driver speaking MCP to that host's DevTools endpoint. The host
encodes each painted frame as a WebP still onto a local socket, and the
server relays the newest one to every viewer over the socket. The client
carries no engine, no driver and no browser profile: it is a viewer.

**The phone is a viewer too.** The same frames, the same bar, the same Take
control: the page paints each frame on a canvas where the desktop's shell
lays a native view. **The canvas keeps the picture's shape**: a wide page in
a portrait phone is scaled to fit with the takeover's own background above
and below it, and taking control lays the page out at the phone's shape, so
the bands fall away. **A finger drives the page the way a mouse does**: a
tap is a press and its release, a one-finger swipe is a wheel, and a tap on
the page raises the phone's keyboard - its named keys, inserted text and IME
compositions cross as the same key and text events the desktop overlay
sends. **The bar wraps on a finger's screen**: a hand-off's row is wider than
a portrait phone, so its actions take a line of their own and **Done**,
**something's wrong…** and **Not now** stay reachable at 393 and 430 points.
Neither client has a WebView engine or an in-process node any more.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| The hand-off's card | `browser hand-off` (and the profile, when one is named); **Open browser**, **Done** and **Not now** before the takeover; while it is up, the card says the browser is open over the window and leaves the actions to the bar | the `browser_hand_off` ask, and the page's own surface state |
| The takeover's bar | **back to chat**, the page's **back** and **forward** arrows, **take control** / **release control**, the title (**the browser** and the profile it drives, or the hand-off's own), **Done**, **something's wrong…** and **Not now**; **try again** while the surface is in its failed state | the page's surface state, the parked hand-off, and the server's viewer state |
| The feedback line | a text field and **send**: the person's words go to the agent as a prompt and the hand-off is declined | the page's own state; the prompt rides the socket |
| The surface | the profile's page, filling the window under the bar: no window, no tab strip, no browser chrome | the server's relayed frames - drawn by the desktop's native overlay, or by the phone's canvas |
| The strip's row | `browser` with its profile count, the work ring while a call runs, a disc when the takeover is up and a flat disc once it has been driven; **show** / **hide** | the server's browser fleet, and the surface state |
| The engine line | `the server's own Chromium`, on both platforms | the server's browser fleet |
| A named profile | its own host, its own cache root, its own logins; closed rows keep their files and can be deleted | the server's profile store |
| The states | `opening the browser…`, the server's sentence when it failed, the host's own death sentence - `the browser host exited (signal 9; pid 1234); the next call brings it back` - when a host dies under the surface (the cause is a signal or an exit code), and the page's own error page when a load fails | the page's surface state, and the server's failure frame |
| A refusal | the server's own words: the profile another seat owns, no free port, the host not answering, or a page area that could not be placed | the attachment's answer and the surface's failure frame |
| An unreadable row | the profile whose owner record cannot be read: drawn with the server's mark and carrying no verb, because the row cannot name an owner to act for it | the server's row |

## The rules

**One host per profile, and the host outlives the surface.** A profile's
browser is a server process of its own, started on its first real need - a
session call, a raise, the strip's show - and never at server start. Closing
the surface ends nothing: the sessions keep driving the same page, and the
next Open shows it as they left it. A profile's close ends its run and keeps
its logins; a delete takes them.

**A profile is a browser of its own, owned by the seat that opened it.** An
omitted profile is the asking seat's own, named after its project and label
and opened on its first call; the browser's own shared profile is reached
only by naming it `@shared`, a word outside the profile charset so it can
never collide with a real profile. A named profile is a directory under the
server's `browser/profiles/<name>/cef`, with an ownership record beside it
that is written on the first open and read back after a restart. The fold
is lossy, and a collision is a refusal by the owner's name, never a share.
A hand-off with no profile raises the asking seat's own - the one its calls
drive - and another seat's name is refused by name. Every call logs the
profile, host pid, port and cache root it drove, so a tab's identity is
readable after the fact. A despawn or a close sweeps the down seat's
profiles with it.

**Profiles moved once, and only once.** The first server that finds no
marker copies the client's own `browser/` tree across - every named profile
and the shared browser's cache, logins, carried page and owner record
included - and writes the marker. A later boot does nothing, and a profile
the server already has is never overwritten by the stale client copy.

**A page context that dies is rebuilt and answered in the client's words**,
never handed back as the driver's `_page` TypeError, and a driver that cannot
attach within its 30 s bound has its host named and ended rather than
reported as a bare timeout.

**Every viewer is a watcher until it takes the role.** A surface attaches to
a profile and receives every frame; **exactly one attached viewer holds the
controller role**, and only its gestures and history arrows reach the page.
The bar's **take control** moves the role to this surface (the previous
holder becomes a watcher at that moment), and **release control** gives it
back with nobody holding it. A watcher's arrows are drawn disabled: the move
is the controller's, and the server refuses one from anyone else. The server
pushes who holds it to every viewer, so two surfaces draw the same truth, and
each knows its own viewer id - the bar reads whose role it is rather than
guessing. **The bar also names the browser it is over** - `shared`, or the
profile's own name - so two devices driving two different browsers cannot
read as one browser with a role held twice.

**The picture is the server's own frame.** The host paints windowless and
encodes a WebP still; the server relays the newest one per profile to each
viewer, and the client decodes it - into a layer at the page area's rect on
the desktop, onto a canvas on the phone. **Both keep the picture's shape**: a
frame laid out for another viewer's surface is fitted with bars rather than
stretched, and taking control lays the page out at this surface's shape so
the bars fall away. There is no screen capture
anywhere, so the Screen Recording grant, the picker and its monthly
re-approval are not part of this surface.

**Show and hide are this client's own.** The rows' marks come from the
server's fleet - a disc means some client has that browser up - but each
row's button reads this page's own surface: a phone shows **show** while the
desktop holds the picture, and lowering here gives only this client's viewer
back.

**A windowless page must stay a visible page.** The host asks CEF for a
visible, focused page (`WasHidden(0)`, `SetFocus(1)`) and drives its frames
from an explicit begin-frame cadence of one every 8 ms - CEF's own scheduling
under an external message pump painted the same page at about half the
screen-refresh rate. The
sessions' tools - navigate, snapshot, click, type, evaluate - are the same
vendored Playwright battery over the same CDP connection; nothing on the
wire changed.

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
over, and **only the controller is sent one**.

**The page's sound plays at the controlling client.** The host relays what
the page plays as PCM packets beside the frames, and the shell plays them on
this machine - the desktop's audio engine, the phone's web audio. A watcher
is looking at the page rather than driving it, so they hear nothing.

**A recording is the agent's, and the person sees its layout.** While an
agent records the page, the page is laid out at the file's own 1280x720 and
put back when the take ends, so the file is the layout the page had rather
than a scaled copy of the surface.

**There is no external-browser door.** Every page lives in the server's
engine and nowhere else; no client hands one to another browser.

**A restart reopens the page the person last saw.** The page a hand-off's
Open or the strip's show puts in front of the person is written beside the
profile, and every browsing call that changes the page rewrites it, so a
later launch - after a restart or an idle close - opens where the session
left off instead of on a blank page. Only a web address is ever reopened.

**A carried page that has not been shown does not lose to a blank one.**
The host opens on `about:blank` and the server navigates to the carried page
once the passkey veto is registered, so a probe can meet the blank page
before the navigation lands. A page written this way is held beside the
profile until an Open or the next successful browsing call reads a real
page; a blank read or the engine's own error page keeps both the marker and
the hold, and the next launch opens the held page. The hold is on disk, so a
restart does not erase the page the marker exists for.

**An idle browser closes on the SERVER's clock.** After `[browser]
idle_minutes` with no call or attach, a named profile's owner is sent
one heads-up; the browser closes `grace_minutes` later unless it is used,
and the shared browser closes quietly at the same total without a message. **A lower does not reset the clock** - the next call or attach
does. A viewer attached is a use, and a hand-off waiting on the
person holds the profile. The close ends the host, not the page.

**The catalogued engine's limits are named, not hidden.** The prebuilt CEF
has no H.264 decoder (the job sites the probe exercised do not need it), it
presents plain Chromium's user agent and brands (a real browser's identity,
not an automation tell), and its frames are software-rendered (measured at 60
frames per second at the surface's size; CPU was not profiled).

**A failure is drawn where the person is looking.** The takeover's page area
carries the server's sentence and the hand-off stays parked; the strip says
what a refused show or hide said. The hand-off's answer shape is unchanged:
**Done** and **Not now** are the two answers - the feedback line sends its
words as a prompt and declines with Not now - and the fourth answer is a
separate cut.

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
dock return with it.
