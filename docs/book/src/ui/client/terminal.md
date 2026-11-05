# The terminal surface

One terminal per seat, in the `forge` server process: the PTY, the screen
model and the scrollback are the server's, the client draws with xterm.js,
and bytes cross the socket as their own binary frames. It is hidden by
default and raised by the strip's **show** or a terminal hand-off's **Open**,
the browser takeover's sibling. The drawing it is held against is
[web-terminal.html](./web-terminal.html), beside this page.

**The terminal belongs to the seat, not the occupant.** It is created lazily
on first use and carries the seat's own cwd and environment - a worker's
terminal runs in its worktree, the lead's in its checkout - and `/new`,
`/resume` and a handoff leave the PTY, its shell and its scrollback
untouched. It lives as long as the seat: releasing the seat ends it, and an
explicit close, the shell exiting, an idle close or forge exiting also ends
it. `[terminal] idle_minutes` closes one nobody is attached to or has called
after that long, and the strip's row goes back to **off**. **A raised surface
belongs to the seat it was opened on**: switching seats lowers it, and the
old terminal keeps running.

**The agent drives it like a human.** There is no agent mode, no wrapper and
no environment flag of its own: the agent types bytes and named keys through
`terminal__write`, runs a command through `terminal__run`, reads the screen
through `terminal__read`, and waits with `terminal__wait` (exact through the
shell's OSC 133 marks, or the heuristic with `source: heuristic`). The
person's own input is the hand-off's or their own **take control**: while
either holds the terminal the tools are refused, and a hand-off hands the
agent the ending rather than the screen. The keystrokes themselves go from
the client to the server to the PTY master, not into the agent's turn; the
shell may echo them, so a read after the hold resolves can carry what was
typed.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| The hand-off's card | `terminal hand-off · terminal N`, the agent's reason, **Open terminal**, **Done** and **Not now** before the surface is up, and `typing is hidden: the program is reading a secret` when the PTY has ECHO off. While the surface is up the card leaves the actions to its bar | the `terminal_hand_off` ask, and the page's own surface state |
| The surface's bar | **back to chat**, `terminal N · <shell> · <cwd>`, the state line - `the agent is running a command`, `waiting for you`, `another client has the keyboard`, `the agent is waiting for input`, `the shell exited`, `the terminal closed`, `you have control`, `view only`, `opening the terminal`, or the server's sentence when it failed - and, while a hand-off is parked, its reason, `typing is hidden`, **Done** and **Not now**; then **read the screen**, **take control** / **release control** when no ask is parked, and **close** | the terminals state push, the record's `terminal`, the page's own surface state and the parked ask |
| The surface | the terminal under the bar, filling the rest: the shell's screen, its colours and its cursor | the attach's `terminal_view` repaint, then the kind-4 output frames |
| The strip's row | one pill, like the browser's: the terminal glyph, the state mark, `terminal`, the state in words - `a command is running`, `waiting for you`, `a password is wanted`, `exited`, `idle`, `off` - and **show** / **hide**, or **start** on a seat with no terminal. **The row is the person's door even before an agent opens one.** On a seat the server holds no session for the row is a note, not a door: `terminal` and `the seat has no session yet - start it first`, because a terminal opened there would run with no record for the page to draw | the session record's `terminal`, the pushed `terminal_changed` update, and the seat read's refusal for the sessionless state |
| The touch key bar | under `pointer: coarse`: Esc, Tab, Ctrl-C, the four arrows, Enter, `/`, `-`, `\|` and `~`, each at a 44px target | the page's pointer capability |
| The states | `opening the terminal…`, the server's own sentence when the attach failed, and the terminal when it is live. Every state is words as well as colour | the page's surface state |

## The rules

**The client is a viewer, and xterm.js is the renderer.** The attach answers
with the visible screen as a VT stream - the server's grid, its colours and
its cursor - and every later frame is the shell's own bytes, so the client
sees exactly what the shell wrote. **The repaint is laid out at the server's
grid**, so a first raise sends the size the client fitted to and draws the
repaint that comes back; frames that land while xterm.js is still loading are
held rather than dropped. The DOM renderer is the default;
`@xterm/addon-webgl` is loaded behind a try so a renderer that will not paint
never draws an empty terminal over a live shell. Fit, unicode11, search,
clipboard (OSC 52) and image (Sixel and iTerm2 IIP) are loaded beside it.
**Kitty graphics is not drawn**; that gap is named rather than hidden, and
Sixel and iTerm2 IIP cover the larger share of CLI image use.

**The keyboard is the terminal's only while the person holds it.** There are
two doors to that hold, and they are the browser's controller role: **take
control** on the bar, which the person uses without an agent asking, and a
parked hand-off, which an agent asks for. One holder at a time across
clients - another client's take overrides it - the agent's tools are refused
while it is held, and lowering the surface or dropping the connection
releases it. **A parked hand-off is one client's keyboard too**: the first
client to type claims it, the bar of any other client reads `another client
has the keyboard`, and the take control door is closed while the ask waits;
the ending returns the keyboard to the agent. Outside both - and on any
client that is not the hand-off's door - the raised surface is a viewer: the
key bar and the terminal's own input are inert, and the server refuses a
keystroke that arrives anyway, while the output frames keep it live. Inside
the hold,
Ctrl-C, Ctrl-D, Ctrl-Z and Ctrl-L reach the PTY untouched; Cmd+J raises and
lowers the surface, Cmd+W lowers it rather than the window, and a browser
tab's Back lowers it, because the surface pushes one history entry. **A finger cannot produce Escape, Tab or an arrow**, so the
touch key bar is required rather than a fallback, every control it offers is
44px, and Ctrl and Alt are sticky keys for the next character the soft
keyboard types.

**Accessibility is honest about its limit.** xterm.js's `screenReaderMode`
exposes the buffer, and **read the screen** puts the same text into a
focusable region, so a reader who cannot use a canvas terminal still gets the
content and anyone can copy it. Every state forge draws is text as well as
colour. A terminal is a grid whose content programs control, so xterm.js
cannot promise WCAG-level semantics for arbitrary curses output; the states
forge draws are the part forge promises. Contrast is checked where the token
set is, for the terminal's own chrome; a program chooses its own colours, so
`ls --color` output is not a promise.

**The theme is the client's token set.** The 16 ANSI colours, the cursor and
the selection are tokens in `client/src/theme.ts` beside the rest, so a
second `[client] theme` palette themes the terminal for free. The component
maps those tokens onto xterm's theme and holds no colours of its own.

**The desktop shell forwards the bytes itself.** On the desktop the page does
not hold the socket, so kind-4 frames reach it as a `client://terminal-frame`
event carrying the id, the sequence and the bytes base64, and the page
writes each frame to xterm as it lands - the frames are the shell's own
bytes, so a frame is a write, not a repaint.

**xterm.js is its own chunk.** The surface is imported when a terminal is
first raised, so a session that never opens one never downloads the
renderer.
