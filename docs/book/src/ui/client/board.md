# Board

One project's board, opened as a takeover at
`/board/{org}/{project}`. It opens on the **window**: what needs the
reader, what looks wrong, who is doing what, and what is next, in that
order. The **full board** of lanes and the **archive** of closed rows are
one toggle away on the same page, never the front door. The fleet stays the
glance; this is the world, and nothing of any other project appears here.
The drawing it is held against is [web-board.html](./web-board.html),
beside this page.

It is reached from two doors: `open board` on the project's row on the
[home](./home.md), and the tasks strip in a session's chat, which carries
`open board` first in its list. Both open the same page. `back` and `done`
return the reader where they came from, and a deep link with nothing to go
back to lands on the home.

The page draws from the `board` subject, which carries only the window's
fields: a row's worked time, its ages, whether its owner is live, its
marks, its wait, its rollup and its epic's subject. The row's `detail`
brief and its transition history are **not** in that read - they load on
demand through the `task_detail` command when a drawer opens, and come back
as a `task_detail` update for that one row - so a board read is bounded by
its row count rather than by how much each row has accumulated. The closed
view is its own subject, `archived`, read once each time the toggle opens.
The seat chips and the fleet's `waiting on you` count come from the home
snapshot beside it; the row and running counts are the board's own.

The page hears the updates that move what it draws: a task write, a
schedule set, a connector set, a drawer's row, and the seat events a row's
liveness is read from. **A task write is applied from its frame, with no
read**: the `tasks_changed` frame carries the project's live set, so a seat
claiming a row, a row being cut, or a move made in another view lands on the
drawn board at once - the derived facts stay what the last read said and the
beat reconciles them. While it is mounted and the page is being watched, it
also re-reads on a slow five-minute beat: a row's time-derived marks are
computed by the server at read time, and no update announces that a window
crossed. A page nobody is watching releases the beat entirely, and the
return re-read the page already makes when it comes back from a hidden
spell of at least two seconds asks the board once through the same guards.
Only the `wrote ... ago` line keeps ticking off the page's own clock; the
card's worked time and the ages on cards advance only on a read, so they
are refreshed at most five minutes after the page is watched again.

## The window

Exceptions first, aggregated by outcome: a reader of a fleet does not want
one card per agent, they want what needs them, what looks wrong, who is
doing what, and what is next. Four regions, in that order, each drawn
under a heading with its count.

### Needs you

The rows waiting on the reader, as cards - a look ahead of a decision,
then the oldest wait first, because a look is a finished row nobody can
close until it is given.

**A card is an ask, not a dossier**: it reads as a decision at a glance,
and the detail sits behind its own small controls. Every card is one quiet
meta line - the kind, the row's subject and owner, and how long the wait
has run, counted from the wait's own start and never from the row's last
edit - then the question, then what the reader does about it.

A **decision card** names the question the lead asked and draws the options
it enumerated as compact buttons the reader answers with one press. The
note is optional and collapsed behind `add a note`; a wait minted before
decisions carried questions instead offers `answer in your words`, which
opens the same box. A `discuss` link goes to the owner's session - where the
conversation happens, and what it reaches is written back to the row.

A **look card** is the same shape with the look's own ask - `Approve
<artifact> or say what to change` - the artifact as a small door, and the
strip's controls: `approve`, `send back`, and the send-back words behind
`add a note`. Two of the core's refusals are drawn rather than offered: a
root whose children are still open cannot close on a look and draws the
count in place of `approve`, and a look with no verified evidence behind
its completion draws that hold; `send back` stays either way.

Where a card carries evidence, it is one summary line: the gate's own word
(`evidence verified`, `evidence unchecked`, `evidence refused`) and the
bounded excerpt, which `more` expands and `less` folds back. `discuss` and
`open the row` sit together as quiet links under the controls.

### Exceptions

What contradicts the state, each a labelled fact rather than a badge:

| Line | Means | Read from |
|---|---|---|
| `forge could not confirm its evidence: <why> - no change since HH:MM` | the row carries an evidence claim the repository could not back | the row's `evidence.problem` and its last touch |
| `<owner> has no session behind it - no change since HH:MM` | the owner is gone | `owner_live` and the last touch |
| `no change since HH:MM` | a held row nobody has touched past the band | the `stale` mark and the last touch |
| `<worker> holds no row since HH:MM - feed it or despawn it` | a live worker with nothing to do | the fleet row's `unaccounted_worker` miss and the seat's last activity |

A row that is also an ask is drawn under needs you and not here: the ask's
own card already states the fact. A seat with no recorded activity says
`holds no row` without a time rather than borrowing the last touch.

**Staleness is a timestamp, not an adjective.** The line says when the row
last changed, in the reader's own clock, because motion is not progress
and a bare `stale` asks the reader to do the arithmetic.

### Live

One line per seat HOLDING work on this project, saying what it holds: the
seat's label (its door into the session), the row's running words and when
the seat was last active, what the row last touched, the measure the row is
next judged by, and how many more rows the seat holds when it holds more
than one. A seat holding nothing is the fleet's own count and the stopped
worker's exception, not a line here. What cannot be known is drawn as
unknown, never as progress.

### Next

The ranked ready queue: the ready and unowned rows in the order the lead
would dispatch them (rank, then creation), each with its estimate - or its
cadence, marked `standing`, for work that never finishes.

## The full board

The lanes face, under the `full board` toggle.

| Region | Shows | Read from |
|---|---|---|
| Top bar | `back`, the project's name with its own read (`N waiting on you`, `N rows`, `N running`), the project's seat chips, `done` | the route and the snapshot |
| The switch | `window`, `full board`, `archived` | the page's own state |
| Waiting on you | the rows whose wait is a decision, each card leading with the wait's ask, with the row's links and a look's artifact beside it; the verify gate (`approve`, a words box, `send back`), or an answer box; a root whose children are not all done draws the count and the send-back instead of `approve` | `waiting_on`, the wait's own start for the age, the row's links, and the row's `rollup` |
| Lanes | every LIVE card, in its state's lane, with the Backlog folded to its ten | the board's `projects[].rows` |
| Create | a subject box, an epic picker and `+ add`, which files the row at the queue's end | `task_create` |

Three lanes, drawn in this order: **Backlog** (pending), **Waiting**,
**In progress**. A terminal row leaves the bulk board the moment it ends -
its state, when it was archived and its `restore` are the `archived`
toggle's - so no Completed, Failed or Canceled lane is drawn here. The pool
of everything not yet started leads, then the lane that wants the reader,
then the work. Backlog is the `pending` status: work kept to look at later,
not by itself an assignment and not a promise to start. A lane is a name,
its count and a rule, with its cards under it - no box around the column -
and the lanes wrap to a second row rather than scroll sideways.

**The Backlog folds.** A queue accumulates for the life of the work, so the
Backlog draws its top ten and the rest waits behind a `show all (N)` control
under the last card. A card the reader is dropping into the folded lane
stays drawn while the core's answer is in flight, and a jump aimed at a
folded row opens the fold and lands on it. Waiting and In progress never
fold.

### A card

Three lines, top to bottom:

- the mark, then the subject. A row in progress displays its `active_form`
  where it has one, with the subject under it.
- the owner (their initial in a disc, linking to their session page, or
  `unclaimed`), the epic it belongs under where it has one, its links, and
  its chips - with a `done/total` rollup beside them on a parent.
- worked time against the estimate with the measure under it, and how long
  since the row last moved (`wrote 12s ago`, breathing while the row is
  running and recently wrote).

**The tree reads in two marks.** A parent's card wears an `epic` pill and a
solid left spine; each child wears the epic's name in a pill with a return
arrow and a dimmer spine. The child's card pill is a door: it brings the
epic's own card into view and flashes it.

The mark carries shape as well as colour, so the board reads without colour.
The live lanes draw the first three; the archived shelf draws the finished
three beside the same row's state word:

| Status | Mark | The state |
|---|---|---|
| Pending | a ring | queued, not started |
| In progress | a filled accent dot | a seat is on it |
| Waiting | a warning square | a wait, a question or the verify gate |
| Completed | a tick | done |
| Failed | a cross | it ended in failure |
| Canceled | a dim bar | withdrawn, kept for the record |

The chips, in words. The five marks and the evidence verdict are the
server's own derivation; the wait's kind and its age, `owner gone` and
`reserved for you` are read here from the row's fields:

| Chip | Tone | Means |
|---|---|---|
| `waiting on you` | warn | a decision wait, or a look park on a row minted `verify: user` |
| `on <row>` | warn | waiting on another row's completion |
| `waiting on a resource` | warn | waiting on something outside the board |
| `waiting <3h>` | bad | the wait's own age has passed the window |
| `overdue` | bad | worked time has crossed the estimate |
| `no change <5h` | dim | untouched past the band, and its owner is still live |
| `owner gone` | bad | its owner is no longer a live seat |
| `in review` | blue | an unmerged pull-request link on a live row |
| `to close` | blue | a root whose children are all done, waiting for its close |
| `reserved for you` | dim | the user reserved this pick: the drive leaves it, it never reads ready, and no claim takes it |

A terminal row carries no chips at all: what it ended as, and the lead's
attestation where one was recorded, are read on the archived shelf.

**The look has two doors, and the row page draws both.** The user's own
`approve` sits on the window's look card and on the strip; a lead who
watched the user approve in chat completes the row through `tasks__approve`,
which requires the approval quoted and records it on the row. The row then
ends, so its archived shelf line says `lead attested`, and its row page -
opened from that line - draws the quote with a `reopen` button: reopening
strikes the attestation and parks the row on the user again. The page's
`pick` line is the other side of the drive: `reserve for you` marks the row
so the drive never dispatches it, and `release` takes that back.

**A wait leads with its ask.** The card and the row page both open on the
wait's own `detail` where it states one, else the row's `detail`, and a wait
with neither falls back to the state's sentence. In a look the artifact is
its own door, `look at <name>`, in the same pill grammar as `approve` and
`send back`; when the artifact is already one of the row's references, the
door is its only showing and the links line keeps the others.

The links come off the row's own list: a URL follows in a new tab under its
label, and a path or a branch draws as text rather than a dead anchor.

## The archive

The `archived` toggle reads the closed view and is where finished work
lives: the terminal rows still in the live set, then the archive, each with
its state mark and word, `lead attested` where the lead recorded one, when
it was archived (a row closed but not yet archived says so), and a `restore`
that brings the row and the tree it took back to the live set. The row's
subject is a door: it opens the row page whole, which is where a completed
row's brief, history, attestation quote and `reopen` are read. The read
happens once per opening; a task write while it is open is not applied to
it, because the closed set is history and the opening is the reader's own
ask.

## Moving a card

The drag is the full board's interaction, and what the card lands on decides
the command: another lane is a move carrying its place (`task_move`), a
position inside its own lane is a re-rank by drop position (`task_rank`), and
a seat chip is an assignment (`task_assign`). A lane's whole column takes the
drop, an empty one included, and the place the drop chose is the priority the
row keeps on either path. A press that never passes six pixels is a click,
which opens the row's own page. A dropped card lands where it was put at once
and the wire's own snapshot reconciles it a moment later; it animates into
place unless the reader has asked for reduced motion.

The keyboard has the same power without a row of buttons: the arrow keys on
a focused card re-order it within its lane, and the owner picker on the card
is a control like any other.

## The row's page

A click - or Enter or Space on a focused card, or `open the row` on a window
card - opens the row over the board. The drawer asks for the row whole
(`task_detail`) as it opens, so its `detail` brief and its history - which
the board's own read leaves out - arrive a moment later and draw where they
belong: its fields (state, owner, estimate, worked, both clocks, its parent,
its `pick` line and its links), **Its tasks** on a parent, **Waiting on
you** when the wait is a decision, the strip's own controls, its **History**
(the row's own transitions, the newest twenty, with `showing the last N of
M` when the tail is capped), and the way into the owner's session. The
button, the scrim and Escape all close it, and the card has the keyboard
back when it goes.

**Every control is one command on the socket, carrying this project on it.**
`task_move` moves a row between lanes and lands it at the drop's place,
`task_rank` re-orders it, `task_assign` changes its owner, `task_verdict`
approves or sends back a verify row, `task_answer` answers a question
(carrying `choice` when the row offered options, and the note in `words`),
`task_create` files a row, `task_reopen` strikes a lead's attestation,
`task_reserve` reserves or releases the pick, `task_restore` brings a closed
row back, and `task_detail` asks for one row whole. A refused edit says so
on the service line rather than silently reverting.

## The states the page can be in

- **A window**, the ordinary case.
- **Before the first frame**: a route can be addressed before the snapshot
  lands; the page says it is reading, and a subscription the server turns
  down says so in its own words instead of reading forever.
- **Unknown**: a project the snapshot does not carry draws its name and one
  line saying this forge does not hold it, with no regions and no create line.
- **Empty**: a project with no live rows says so under the window; the full
  board draws its three empty lanes and the create line.
- **The archive reading, refused, or empty**: the toggle says which.

## The two widths

Checked at **1600** and at **430**, against the drawing beside it. The window's
need cards are a grid of `minmax(330px, 1fr)` columns, so a wide window sets
two asks side by side and a phone stacks them; the lanes are
`minmax(260px, 1fr)` columns, so a phone stacks them too, and neither scrolls
sideways. A phone's top bar keeps the count that asks something of the reader
and drops the row totals. Every control a finger reaches takes a 44px target
under `@media (pointer: coarse)`, and the drag is a pointer gesture, so it is
the same one under a finger.