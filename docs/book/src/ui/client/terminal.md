# The terminal surface

One terminal per seat, in the `forge` server process: the PTY, the screen
model and the scrollback are the server's, the client draws with xterm.js,
and bytes cross the socket as their own binary frames. It is hidden by
default and raised by the strip's **show** or a terminal hand-off's **Open**,
the browser takeover's sibling. The drawing it is held against is
[web-terminal.html](./web-terminal.html), beside this page.

**The terminal belongs to the seat, not the occupant.** It is created lazily
on first use and carries the seat's own cwd and environment - a worker in a
git tree runs in its worktree under the tree it was spawned on, a worker in
a non-git tree in that tree; the lead in the project directory - and `/new`,
`/resume` and a handoff leave the PTY, its shell and its scrollback
untouched. It lives as long as the seat: releasing the seat ends it, and an
explicit `terminal__close`, the shell exiting, an idle close or forge exiting
also ends it. `[terminal] idle_minutes` closes one nobody is attached to, no
hand-off is waiting on, no client holds the keyboard of, and nothing has
called after that long, and the strip's row
goes back to **off**. **A raised surface
belongs to the seat it was opened on**: switching seats lowers it, and the
old terminal keeps running.

**The agent drives it like a human.** There is no agent mode, no wrapper and
no environment flag of its own: the agent types bytes and named keys through
`terminal__write`, runs a command through `terminal__run`, reads the screen
through `terminal__read`, and waits with `terminal__wait` (exact through the
shell's OSC 133 marks, or by the quiet-screen heuristic, which the answer
labels `state.source: heuristic`). The
person's own input is the hand-off's or their own **take control**: while
either holds the terminal the tools are refused, and a hand-off hands the
agent the ending rather than the screen. The keystrokes themselves go from
the client to the server to the PTY master, not into the agent's turn; the
shell may echo them, so a read after the hold resolves can carry what was
typed.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| The hand-off's card | `terminal hand-off · terminal N`, the agent's reason, **Open terminal**, **Done** and **Not now** before the surface is up, and `typing is hidden: the program is reading a secret` when the PTY has ECHO off. Once the surface is live the card leaves the actions to its bar | the `terminal_hand_off` ask, and the page's own surface state |
| The surface's bar | the browser takeover's own bar: **back to chat**, `terminal N · <shell> · <cwd>`, the state line - `the agent is running a command`, `waiting for you`, `another client has the keyboard`, `the agent is waiting for input`, `the shell exited`, `the terminal closed`, `you have control`, `view only`, `opening the terminal`, or the server's sentence when it failed or refused a take - and, while a hand-off is parked, its reason, `typing is hidden`, **Done** and **Not now**; then **take control** / **release control** when no ask is parked (`taking control...` / `releasing...` while the server answers). **No other door**: back to chat is the close, and the screen is read through the terminal's own accessibility tree | the terminals state push, the record's `terminal`, the page's own surface state and the parked ask |
| The surface | the terminal under the bar, filling the rest: the shell's screen, its colours and its cursor | the attach's `terminal_view` repaint, then the kind-4 output frames |
| The strip's row | one pill, like the browser's: the terminal glyph, the state mark, `terminal`, the state in words - `a command is running`, `waiting for you`, `a password is wanted`, `exited`, `idle`, `off` - and **show** / **hide**, or **start** on a seat with no terminal. **The row is the person's door even before an agent opens one.** On a seat with no working tree - the read refused, a worker never spawned, a slot naming no project - the row is a note, not a door: `terminal` and `the seat has no session yet - start it first`, because a terminal opened there would run with no record for the page to draw | the session record's `terminal`, the pushed `terminal_changed` update, and the seat read's refusal for the no-working-tree state |
| The touch key bar | under `pointer: coarse`: Ctrl and Alt (sticky), Esc, Tab, Ctrl-C, the four arrows, Enter, `/`, `-`, `\|` and `~`, each at a 44px target | the page's pointer capability |
| The states | `opening the terminal…`, the server's own sentence when the attach failed, and the terminal when it is live. Every state is words as well as colour | the page's surface state |

## The rules

**The client is a viewer, and xterm.js is the renderer.** The attach answers
with the visible screen as a VT stream - the server's grid, its colours and
its cursor - and every later frame is the shell's own bytes, so the client
sees exactly what the shell wrote. **The attach carries the size the client
fitted to**, so the repaint comes laid out for the client's own grid and is
drawn at once - a repaint for another grid would have to be skipped and asked
for again, which is what read as the screen flushing on a second client.
Frames that land while xterm.js is still loading are
held rather than dropped, up to a 1 MB bound, past which the surface asks for
a fresh repaint instead of drawing a fragment. The DOM renderer is the default;
`@xterm/addon-webgl` is loaded behind a try so a renderer that will not paint
never draws an empty terminal over a live shell. Fit, unicode11, search,
clipboard (OSC 52) and image (Sixel and iTerm2 IIP) are loaded beside it.
**Kitty graphics is not drawn**; that gap is named rather than hidden.

**The keyboard is taken explicitly and lost implicitly.** The person presses
**take control** on the bar, and only then do the key bar and the terminal
take their keys. One holder at a time across clients - another client's take
overrides it - and the agent's own call takes it back from the person.
**Every loss is said**: a client that lost the role reads `another client has
the keyboard` (or `the agent took the keyboard`), so nobody types into a
keyboard they no longer hold. **Going back to chat gives the keyboard up**,
and so does the connection ending; a lower is not something a terminal keeps
the agent out with. **A parked hand-off raises the surface as a viewer**:
take control there to type, and the ending returns the keyboard to the agent.
Outside the role the raised surface is a viewer: the key bar and the
terminal's own input are inert, and the server refuses a keystroke that
arrives anyway, while the output frames keep it live. Inside the role,
Ctrl-C, Ctrl-D, Ctrl-Z and Ctrl-L reach the PTY untouched. **Cmd+J raises and
lowers the surface on the desktop app**, on a seat whose read answers, where
the page handles it; a browser tab keeps that chord for itself, so there the
browser's Back lowers the surface, as the hardware Back does on the phone,
because the surface pushes one history entry. **Cmd+W is the window's on the
desktop and the tab's in a browser**; on the phone's hardware keyboard,
where neither takes it, it lowers the surface. It is never sent to the
shell. **A finger cannot produce Escape, Tab or an arrow**, so the
touch key bar is required rather than a fallback, every control it offers is
44px, and Ctrl and Alt are sticky keys for the next character the soft
keyboard types.

**Accessibility is honest about its limit.** xterm.js's `screenReaderMode`
exposes the buffer as an accessibility tree, so a reader who cannot use a
canvas terminal still gets its content; the bar carries no second copy of the
screen, the same way the browser takeover has none, and both are read through
their own surface. The raised surface is a dialog (`role="dialog"`,
`aria-modal`), its state line is a live region, and every state forge draws
is text as well as colour. A terminal is a grid whose content programs
control, so xterm.js cannot promise WCAG-level semantics for arbitrary curses
output; the states forge draws are the part forge promises. The surface is
not a Tab trap: Tab inside the terminal is the PTY's key, so a trap could not
both leave it to the shell and confine focus, and the bar's controls stay in
the document order instead. Contrast is checked where the token
set is, for the terminal's own chrome; a program chooses its own colours, so
`ls --color` output is not a promise.

**The theme is the client's token set.** The 16 ANSI colours, the cursor and
the selection are tokens in `client/src/theme.ts` beside the rest, so a
second `[client] theme` palette themes the terminal for free. The component
maps those tokens onto xterm's theme, with a fallback hex for each token a
theme does not set and nothing else of its own.

**The desktop shell forwards the bytes itself.** On the desktop the page does
not hold the socket, so kind-4 frames reach it as a `client://terminal-frame`
event carrying the id, the sequence and the bytes base64, and the page
writes each frame to xterm as it lands - the frames are the shell's own
bytes, so a frame is a write, not a repaint.

**xterm.js is its own chunk.** The surface is imported when a terminal is
first raised, so a session that never opens one never downloads the
renderer. A chunk that will not load lowers the surface instead of leaving
it opening: the strip's row is its door again, and the next show retries.
