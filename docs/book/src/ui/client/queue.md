# The queue pile

The prompts waiting in the CLI's queue, drawn as a pile above the box on
[a session page](./session.md). It is the drawing
[web-queue.html](./web-queue.html) beside this page, every variant of it.

**The pile is the core's, not the page's.** The seat's read carries it and
the stream keeps it current, so two clients agree about what is waiting;
the page draws it and dispatches a cancel. A row leaves when the core says
it left, so a prompt the CLI has already taken cannot be drawn as dropped.

## What it draws

| Part | Shows | Read from |
|---|---|---|
| Head | `queued` and the count, then the key hints: `↑↓ walk`, and `⌫ cancel` once the walk is on a row | the queue's length |
| Face | the walked row's words and meta: the source chip, `next` on the oldest, `#i / n` while walking | `queue[].text`, `queue[].source`, the walk's cursor |
| Arcs | every other queued prompt as one 16px strip of its own card behind the face | the rows behind the face |
| Ended | one dim line where the last card was, for a row that left without being delivered | `queue_ended` |

**One mechanism makes the depth.** Every row sits at the pile's foot,
lifted one step per queued prompt, so the rows behind the face show as
edges rather than as a list. The face is the walked row - the newest when
the walk is in the box - and only the face draws its words. The step is
wide on purpose (Ved, 2026-10-04): at six pixels the edges read as one
smudge, and every queued prompt keeps a strip of its own.

**The arithmetic has no cap.** A pile deeper than the room draws deeper
than the room, on purpose: a cap would hide waiting prompts, which is the
opposite of what the pile is for. The chat column above shrinks by exactly
the pile's height, so nothing paints over it.

**The source chip names where a prompt came from**, in its own colour:
you, `cron`, `gotify`, `slack`, and `peer` for another session's envelope
- whose card draws its body alone, since the envelope's raw head is
plumbing and the sender is the chip's own word. Forge's own machinery - a
worker kick, an auto-continue - carries `forge`, which is neither the
reader's nor a connector's.

**A row that left without being delivered says so.** `discarded, never
sent` and `refused by a hook` are the two endings the core reports with
words; a row that was taken, or one the reader cancelled, has said what it
had to say by leaving.

## The walk

Up, from the box, enters at the newest and walks older until the oldest
queued prompt; one more down past the newest lands back in the box, focus
included, so the next Enter is the next send. Escape is the same return.
`⌫` cancels the walked row; the pile hands the keyboard back to the box
when its last row leaves, so a keystroke is never dropped on the body.

## The wire

The pile follows the CLI's own lifecycle frames, measured on live
captures:

- a prompt written under a uuid answers `command_lifecycle` `queued` 1-5ms
  after the write - the card enters the pile;
- `started` lands at the turn boundary that delivers it, 2-3ms after, and
  the card drains into the chat as the row it becomes, carrying its wait
  under it (`queued 0:0n · sent`). While it waited, the card was the only
  drawing of the words: the row is held until this frame;
- `completed` says the turn that carried it ended.

A cancel dispatches `cancel_async_message`; the CLI answers
`{"cancelled":true}`, and a cancel landing after `started` answers false -
the card has usually drained by then.

**A parked question is the pile's other shape.** Every prompt arriving
while a question is up is `queued` and stays; the answer lands and all of
them go `started` in the same instant, delivered in the request that
carries the answer.

**All of this carries only under forge's uuid stamp** - the CLI emits no
lifecycle frames for an unstamped prompt (measured: 0 frames without a
uuid, 6-9 with), which is why every send stamps one. The live side of this
is the `queued_prompt_lifecycle` scenario in the wire harness.

## The two widths

The pile is drawn and checked on the session page's column, at 1600 and at
430, where its cards take the column's width and the words truncate rather
than wrap. Every card control takes a finger's height under a coarse
pointer, per the composer's own rule.
