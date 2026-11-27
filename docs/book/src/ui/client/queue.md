# The queue pile

The prompts waiting in the CLI's queue, drawn as a pile above the box on
[a session page](./session.md). It is the drawing
[web-queue.html](./web-queue.html) beside this page, every variant of it.

**The pile is the core's, not the page's.** The seat's read carries it and
the stream keeps it current, so two clients agree about what is waiting;
the page draws it and dispatches a cancel. A row leaves when the core says
it left, so a prompt the CLI has already taken cannot be drawn as dropped.

**A person's own press joins the pile.** From the moment Enter is pressed
the words are a card, saying `sending` until the core answers; the core's
`prompt_queued` row takes that card over by id, and from that handover on a
message is not drawn in the queue and the chat at once. It enters the chat
at `sent`. A page read that lands inside the handover window can carry the
same words before the queue's retraction pulls them back, so the one home
is the settled state rather than every instant of the crossing.

## What it draws

| Part | Shows | Read from |
|---|---|---|
| Head | `queued` and the count, then the key hints: `↑↓ walk`, and `⌫ cancel` once the walk is on a row | the pile's length, this client's pending sends included |
| Face | the walked row's words and meta: the source chip, its state (`sending`, `no word yet`), `next` on the oldest, `#i / n` while walking | `queue[].text`, `queue[].source`, the pending send, the walk's cursor |
| Arcs | every other queued prompt as one 16px strip of its own card behind the face | the rows behind the face |
| Ended | one dim line where the last card was, with the reason and the prompt's words, for a row that was not sent | `queue_ended`, a refused pending send |

**The card's state is one vocabulary.** `sending` is this client's word for
a press the core has not answered; `queued` is the core's, drawn by the
row's place in the pile rather than as a chip; `sent` is where the row
leaves for the chat. **An accepted send that hears nothing for thirty
seconds says `no word yet`** - uncertainty, never failure: nothing is
auto-failed by the clock, and the next authoritative read settles it.

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
you, `cron`, `gotify`, `slack`, `board` for the task board's chase, and
`peer` for another session's envelope - whose card draws its body alone,
since the envelope's raw head is plumbing and the sender is the chip's own
word. Forge's own machinery - a worker kick, an auto-continue - carries
`forge`, which is neither the reader's nor a connector's.

**A row that was not sent says so, in its own words, with the reason.**
The line is `not sent · <reason>: <words>`, one shape for every path - a
session that ended with the row waiting, a hook that refused it, a CLI that
discarded it, and a connection that went down under a press. The reason is
the core's own sentence. **A retry is offered where re-sending is safe** -
the reader's own send and a cron fire - and not for a peer's or the board's
words, which are not this view's to send again. The retry dispatches the
same words under a fresh id: the first attempt is dead, so the retry is a
new prompt that happens to say the same thing.

## The walk

Up, from the box, enters at the newest and walks older until the oldest
queued prompt; one more down past the newest lands back in the box, focus
included, so the next Enter is the next send. Escape is the same return.
`⌫` cancels the walked row, or takes back a press the core has not listed
yet. The pile hands the keyboard back to the box when its last row leaves,
so a keystroke is never dropped on the body.

## Motion

**The card and the row are the same item in two views.** Acceptance moves
it rather than popping one out and another in: a card's last drawn position
is remembered while it is on the pile, and the row it becomes lands from
there, so the eye follows one thing that moved. A row that arrived with no
card to move from rises into place; a row the reader scrolled to arrives
still. A state chip crosses in place over a reserved width, so `sending`
becoming `no word yet` never walks the chips beside it, and the chat's
scroll position is not touched when a card leaves.

**Reduced motion keeps the position change and drops the animation**, the
same preference the scroll glide reads: the row appears where it belongs
rather than travelling there.

## The wire

The pile follows the CLI's own lifecycle frames, measured on live
captures, and the core folds them into one vocabulary:

- a prompt written under a uuid answers `command_lifecycle` `queued` 1-5ms
  after the write - the card enters the pile;
- `started` (the same `command_lifecycle` frame) lands where the CLI takes
  the prompt out of its queue: at the turn boundary that delivers it, or
  absorbed into the running turn. The core reports it as `sent`; the card
  drains into the chat as the row it becomes, carrying its wait under it
  (`queued 0:0n · sent`), or the core's own note where no CLI frame was
  coming;
- `completed` is the turn's, not the message's: turn outcomes stay
  turn-level. A prompt the CLI settles without running reports `not_sent`
  with the reason.

**The state crosses as one of three, with where it came from.** `queued`,
`sent` and `not_sent { reason }`; a state the core drew itself because no
CLI frame was coming carries the sentence the row's note shows, so a
fallback never reads as the CLI's own word.

A cancel dispatches the client's own `cancel_queued_prompt`, which the
core carries to the CLI's `cancel_async_message` control; the CLI answers
`{"cancelled":true}`, and a cancel landing after `started` answers false -
the card has usually drained by then.

**Every message rides this one queue, whoever sent it.** A person's send, a
peer message, a cron fire, a Gotify push, a Slack delivery, a review nudge,
the task board's chase and forge's own continuation are all written to the
CLI under an id the moment they arrive, and each draws its row where the
CLI starts it - the board's chase included, which is queue-first like every
other delivery and enters the chat at `sent` on the normal path (a route
that never reached the CLI, or a CLI that reports no lifecycle frames,
draws its row at dispatch). A delivery draws at its own `sent`, so one
arriving mid-turn is visible in the pile while it waits,
and its row lands where the CLI took it: inside the running turn when the
CLI absorbs it there, or as the next turn's first row when it queues it to
the boundary. Forge's own machinery keeps its dispatch row on the wire; a
CLI with no lifecycle frames has no `sent`: there each row draws at
dispatch, or on the init frame when it was already waiting. The typed
update a delivery also sends (`peer`, `cron`, `gotify`, `slack`) carries
the envelope's facts and pairs a cron's schedule with its row; it is never
a row of its own - except the delivery-failure notice's `peer` update,
which is exactly the row the server forges for it.

**A delivery parked for a sleeping seat keeps its id.** The uuid is minted
when the delivery is accepted and parked, and the drain dispatches under
that same id, so the queue card, the chat row and the transcript row are
one identity from acceptance to landing rather than two that are later
re-keyed.

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
