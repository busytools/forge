# The client's surfaces

The Svelte app that connects to a running forge and draws it. Its
surfaces have pages here in the same way the TUI's do: read first,
sketched with the change, checked afterwards, current state only, and the
page arrives in the same change that lands the surface rather than as a
follow-up.

Each surface's drawing sits beside it, in this directory, rather than in
`docs/mockups/`, so a page and the thing it is held against are one
place.

## The surfaces that exist

| Surface | Page | Drawing |
|---|---|---|
| Connect | [connect.md](./connect.md) | `web-connect.html` |
| Home | [home.md](./home.md) | `web-home.html` |
| Board | [board.md](./board.md) | `web-board.html` |
| Dictation models | [dictate-models.md](./dictate-models.md) | `web-dictate-models.html` |
| The queue pile | (session page pending) | `web-queue.html` |

The session page, the chat and the composer are built; their drawings are
here and their pages are still to land. `web-session.html` declares six
sections and the client's plan owns four of them: the fifth is the
composer's blocking states and the sixth is the diff review overlay,
which is deliberately not built.

`web-queue.html` is the queued-prompt pile that draws above the composer:
the variants it can be in, and the wire's own lifecycle it follows. It
takes the session page's tokens, its `.strip` neighbours and its own
spine, and its page lands with the session page's set.

## The spine

**A vertical rule with its items hanging off it is the client's
replacement for the terminal's indentation and connector tree.** Ved,
2026-09-29: *"webconnect spine kind of represents what I was looking for
from the TUI. I like the tree structure that I used in the TUI, and I've
been looking for a replacement. I think spine fits very nicely."*

It is a device rather than a screen, and it is already in the design: the
chat draws one on a user turn, as an accent left rule, and that is the
same family.

**Use it where a relationship is a run rather than a nesting**, and reach
for it instead of an indent. Where it is proposed to go, per surface:

- **The chat's turn**, where it already is.
- **The composer's dock**, where prompt requests queue behind one another
  and the queue is a short run.
- **The home's worker rows**, as an option rather than a change: the
  indent under a project says the hierarchy already, and the home has a
  drawing it is measured against. A spine there would be a redraw, not a
  device reaching a new place.

The connect screen's rejected direction uses it too, as the state spine,
and that is the same idea applied to a sequence rather than a
containment.

## Touch

**Touch is a hard requirement, not a fallback.** The client runs on
Android as well as the desktop, so every control a finger reaches takes a
44px target under `@media (pointer: coarse)` - keyed on the pointer, not
the width, because an Android tablet is wide and still finger-driven. A
rail row or a home row carries its link across the row's own height rather
than the height of its text, so the whole row is the target. No affordance
lives on hover alone: hover recolours a control rather than uncovering
one, and the chevron the home row fades in is decoration, not a way in.
The rail's close chip uncovers on hover, and it is the exception that keeps
the rule: a touch screen keeps it shown, a keyboard focus reveals it, and
its rest state takes no pointer - so it is reachable either way, and a tap
on the row lands on the row.
