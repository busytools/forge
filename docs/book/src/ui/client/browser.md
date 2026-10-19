# The browser surface

The client's own Chromium, as a full-window takeover under a slim client bar.
A browser hand-off's **Open** and the strip's browser row both open it; the
sessions drive the same page while it is closed, and opening it shows exactly
what they left. The drawing it is held against is
[web-browser.html](./web-browser.html), beside this page.

**The desktop's engine is the app's own.** A host process - the same bundle
executable in its `--cef-host` mode - runs one windowless (offscreen) browser
per profile, owns that profile's DevTools port, and publishes its frames in a
local shared-memory ring. The client's UI process draws the active profile's
frames in a layer over the window's page area and forwards the person's
pointer and key events to the host over CDP, where the page receives them as
trusted input. The machine's Brave is no longer launched, raised or captured:
the engine, its profile and its picture all belong to the app.

**Android is untouched.** The phone keeps its system WebView engine, its
in-app node and its takeover bar; every rule on this page about the host
process is the desktop's.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| The hand-off's card | `browser hand-off` (and the profile, when one is named); **Open browser**, **Done** and **Not now** before the takeover; while it is up, the card says the browser is open over the window and leaves the actions to the bar | the `browser_hand_off` ask, and the page's own surface state |
| The takeover's bar | **back to chat**, the hand-off's title, **Done**, **something's wrong…** and **Not now** | the page's surface state and the parked hand-off |
| The feedback line | a text field and **send**: the person's words go to the agent as a prompt and the hand-off is declined | the page's own state; the prompt rides the socket |
| The surface | the profile's page, filling the window under the bar: no window, no tab strip, no browser chrome | the host's ring, drawn by the shell over the page area |
| The strip's row | `browser` with its profile count, the work ring while a call runs, a disc when a window is up and a flat disc once it has been driven; **show** / **hide** | the host's own rows, and the surface state |
| The engine line | `the app's own Chromium` on the desktop, `the app's own WebView` on the phone | the page's own platform |
| A named profile | its own host, its own cache root, its own logins; closed rows keep their files and can be deleted | the shell's profile list |
| The states | `opening the browser…`, the shell's sentence when it failed, and the page's own error page when a load fails | the page's surface state |
| A refusal | the shell's own words: the profile another seat holds, no free port, the framework missing, the host not answering | the command's rejection |

## The rules

**One host per profile, and the host outlives the surface.** A profile's
browser is a process of its own, launched on its first real need - a session
call, a raise, the strip's show - and never at app start. Closing the surface
ends nothing: the sessions keep driving the same page, and the next Open
shows it as they left it. A profile's close ends its run and keeps its
logins; a delete takes them.

**A profile is a browser of its own.** The shared profile is the unnamed
call's; a named profile is a directory under `browser/profiles/<name>/cef`,
owned by the seat that opened it, with the ownership record beside it. A
hand-off raises only the asking seat's profile; another seat's is refused by
name. The ownership locks and the single-owner rule are unchanged from the
vendored browser's day - what changed is the engine behind them.

**The picture is the host's own buffer.** The host paints into a POSIX
shared-memory ring; the client maps it and draws the latest frame over the
window's page area, in a layer at exactly the page's rect. The page under it
is the measuring stick and the state drawer; while a frame is live the layer
covers it. There is no screen capture anywhere, so the Screen Recording
grant, the picker and its monthly re-approval are not part of this surface.

**A windowless page must stay a visible page.** The host asks CEF for a
visible, focused page (`WasHidden(0)`, `SetFocus(1)`) and drives its frames
from an explicit 60 Hz begin-frame cadence - CEF's own scheduling under an
external message pump painted the same page at about half that rate. The
sessions' tools - navigate, snapshot, click, type, evaluate - are the same
vendored Playwright battery over the same CDP connection; nothing on the
wire changes.

**Input is trusted input.** The person's mouse, wheel and keys are mapped to
CDP `Input.*` events at the page's own scale: a click is a full pointer
sequence, a wheel is a pixel delta, printable text is inserted as text, the
specials (Enter, Tab, the arrows, Escape) are key events, and Cmd+V reads the
pasteboard as text. Widgets that gate on trusted input commit, which is the
whole reason the events are replayed rather than synthesised in the page.

**The native edges have their own doors.** A click on a file input opens the
client's own NSOpenPanel - the app the person is looking at, so the panel
takes focus - and the chosen paths are set on the input over CDP, so the page
runs its own `change` handler. Cmd+V is a text paste. IME, a rich clipboard
and context menus are not part of this cut.

**The catalogued engine's limits are named, not hidden.** The prebuilt CEF
has no H.264 decoder (the job sites the probe exercised do not need it), it
presents plain Chromium's user agent and brands (a real browser's identity,
not an automation tell), and its frames are software-rendered (measured at 60
frames per second at the surface's size; CPU was not profiled).

**A failure is drawn where the person is looking.** The takeover's page area
carries the shell's sentence and the hand-off stays parked; the strip says
what a refused show or hide said. The hand-off's answer shape is unchanged:
**Done** and **Not now** are the two answers - the feedback line sends its
words as a prompt and declines with Not now - and the fourth answer is a
separate cut.

## The widths

The takeover is the whole window, under a bar that is one row at any width.
Narrow, the bar's title truncates and its actions keep their targets; the
page area is the rest. The strip and the dock are behind the takeover while
it is open and return with back to chat.
