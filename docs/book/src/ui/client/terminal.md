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
`/resume`, a handoff and a release/resume leave the PTY, its shell and its
scrollback untouched. Only the seat ending, an explicit close, the shell
exiting or forge exiting ends it. `[terminal] idle_minutes` closes one nobody
is attached to or has called after that long, and the strip says why.

**The agent drives it like a human.** There is no agent mode, no wrapper and
no environment flag of its own: the agent types bytes and named keys through
`terminal__write`/`terminal__run`, reads the screen through `terminal__read`,
and waits with `terminal__wait` (exact through the shell's OSC 133 marks, or
the heuristic with `source: heuristic`). The person's own input is gated by
the hand-off, and only that: the agent is blind while it is parked, their
keystrokes go client to server to the PTY master and nowhere else, and the
tool gets the ending rather than the screen.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| The hand-off's card | `terminal hand-off · terminal N`, the agent's reason, **Open terminal**, **Done** and **Not now** before the surface is up, and `typing is hidden: the program is reading a secret` when the PTY has ECHO off. While the surface is up the card leaves the actions to its bar | the `terminal_hand_off` ask, and the page's own surface state |
| The surface's bar | **back to chat**, `terminal N · <shell> · <cwd>`, the state line - `the agent is running a command`, `waiting for you`, `the shell exited`, `opening the terminal…`, or the server's sentence when it failed - **read the screen**, and **close** | the terminals state push, the record's `terminal`, and the page's own surface state |
| The surface | the terminal under the bar, filling the rest: the shell's screen, its colours and its cursor | the attach's `terminal_view` repaint, then the kind-4 output frames |
| The strip's row | `terminal` with the state in words - `a command is running`, `waiting for you`, `a password is wanted`, `exited`, `idle` - and **show** / **hide**. **The row is drawn only while the seat has a terminal** | the session record's `terminal`, and the pushed `terminal_state` |
| The touch key bar | under `pointer: coarse`: Esc, Tab, Ctrl-C, the four arrows, Enter, `/`, `-`, `\|` and `~`, each at a 44px target | the page's pointer capability |
| The states | `opening the terminal…`, the server's own sentence when the attach failed, and the terminal when it is live. Every state is words as well as colour | the page's surface state |

## The rules

**The client is a viewer, and xterm.js is the renderer.** The attach answers
with the visible screen as a VT stream - the server's grid, its colours and
its cursor - and every later frame is the shell's own bytes, so the client
sees exactly what the shell wrote. The DOM renderer is the default;
`@xterm/addon-webgl` is loaded behind a try so a renderer that will not paint
never draws an empty terminal over a live shell. Fit, unicode11, search,
clipboard (OSC 52) and image (Sixel and iTerm2 IIP) are loaded beside it.
**Kitty graphics is not drawn**; that gap is named rather than hidden, and
Sixel and iTerm2 IIP cover the larger share of CLI image use.

**The keyboard is the terminal's while it is focused.** Ctrl-C, Ctrl-D,
Ctrl-Z and Ctrl-L reach the PTY untouched; Cmd+W lowers the surface rather
than the window; a browser tab's Back lowers it, because the surface pushes
one history entry. **A finger cannot produce Escape, Tab or an arrow**, so
the touch key bar is required rather than a fallback, and every control it
offers is 44px.

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
second `[client] theme` palette themes the terminal for free. No palette
lives inside the component.

**The desktop shell forwards the bytes itself.** On the desktop the page does
not hold the socket, so kind-4 frames reach it as a `client://terminal-frame`
event carrying the id, the sequence and the bytes base64, and the page
decodes them into xterm. The batching that keeps a 60 fps stream of small
writes from becoming 60 IPC calls is the page's, where the animation frame is.
