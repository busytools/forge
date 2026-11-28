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

The header has its own rhythm: the breadcrumb (`back` and the project's
name) shares one line and one baseline with the seat chips and `done`, the
counts (`N waiting on you`, `N rows`, `N running`) are a quiet line of their
own under it, and the window/full board/archived tabs sit below with clear
space between.

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
doing what, and what is next. Above the four regions sits **since your last
look**, the digest strip; the four follow in order, each drawn under a
heading with its count.

### Since your last look

The digest strip summarizes what changed while the reader was away: **one
line per count** - merged, moved, landed, expired - and the rotating sample
of agent actions nobody escalated, each line opening onto the rows it
counts. A moved item names what happened to the row (`answered: ship it`),
a landed one names how it ended, and an expired ask keeps its record on the
expired line, drawn as expired rather than as a bare cancel. The sample is
drawn from the sessions record: a few unasked tool calls with their own
words, so it names commands and paths, and it says what it drew (`sampled 3
of 41 agent actions`). Board reads are left out of it; a board write - an
approve, an answer - samples like any other judgement call. The rotation
moves every five minutes, so a reader who comes back sees a different few.

**The last look is the server's stamp**, recorded when the reader leaves
the window rather than when it opens - a stamp on open would erase the
delta before it drew. Nothing in the strip is urgent; the urgent things
stay in the regions below it.

### Needs you

The rows waiting on the reader, as cards - looks first, because a look is a
finished row nobody can close until it is given, then every ask by its
wait's own age whichever kind it is. **A row waiting on the reader is
blocked on them**, not annotated: the wait is the row's blocker, and the
board is the non-immediate channel for it - an ask needed now goes to the
person directly, and an ask that is not immediate is raised here.

**A card is an ask, not a dossier**: it reads as an ask at a glance, and
the detail sits behind its own small controls. Every card is one quiet meta
line - the kind, the row's subject and owner, how long the wait has run
(counted from the wait's own start, never from the row's last edit), and
its lifecycle chips - then the question, then what the reader does about
it.

There are five kinds, and the ask's kind decides the value the answer
carries:

| Kind | The ask | The bar |
|---|---|---|
| a look | `Approve <artifact> or say what to change` | `approve` (filled), `send back` (outlined), the send-back words behind `add a note` |
| decision | the question, with the options the lead enumerated | the options as filled pills, answered with one press; the note optional |
| clarification | a question whose answer is the reader's own words | `answer in your words` opens the box, `answer` sends it as text |
| exception | something the run could not settle | `retry`, `rescope`, `drop`, `diagnose` as filled presses, one disposition |
| teaching | a recurring trouble and the class it belongs to | `write the lesson` opens the box, `settle the class` sends the words as the answer and the lesson |

**Every word box on the board carries the composer's own dictation**: the
send-back field, the answer box, the lesson box and the note all wear the mic,
a take draws its bar in the field's place while it runs, and its words land
back in the box that began them - one take per page, the same rule the
composer and the review page keep.

A **teaching's** answer writes a project policy note for its class: the
project's leads read it from their next spawn (a resumed lead sees it after
its next spawn or its own `tasks__policy` read), and the drive stops
raising that class. A **look card** also draws two of the core's refusals
rather than offering the press: a root whose children are still open cannot
close on a look and draws the count in place of `approve`, and a look with
no verified evidence behind its completion draws that hold; `send back`
stays either way.

**Every card carries its lifecycle in words, and the words are the
person's.** A deadline chip names the default that runs if the ask is
ignored and when - `escalate at 14:02`, `expire at 14:02`, `reask at
14:02`; never approve, and every ask and look carries one. An ask the
reader has seen says `seen` in the meta line, and one that stood unanswered
past the drive's reminder says `seen, forgotten`; one they deferred says
`snoozed`; all keep their card, because hiding it would be the board
deciding for the person. The footer is one row of small quiet controls:
`discuss` and `open the row` stay the quietest, `mark seen` is the ghost of
the pair and turns into a pressed `seen` pill with a tick once pressed, and
`snooze` is one control whose small menu offers `1h` and `until it moves` -
once chosen it reads the deferral it wrote. Acknowledging and snoozing are
the reader's own acts: no agent has a verb for either, so the press that
writes them is always the person's.

Where a card carries evidence, it is one summary line: the gate's own word
(`evidence verified`, `evidence unchecked`, `evidence refused`) and the
bounded excerpt, which `more` expands and `less` folds back. A claim with no
excerpt still states its word.

**One action bar per card, and the hierarchy is by shape.** The primary
actions - `approve` on a look, the option pills on a decision, a
disposition on an exception, `answer` and `settle the class` on the kinds
whose value is words - are the only filled controls; `send back` is
outlined at the same height and radius; the note is a ghost button, a
button rather than an underline. The row's own doors are not actions: they
sit in a hairline-separated footer as content-sized pills at a full pill
radius, one 8px gap, folding onto a second row on a narrow card rather than
splitting a label - `discuss`, `open the row`, `mark seen` (or the pressed
`seen`), and the `snooze` menu.

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
| Top bar | `back` and the project's name on the breadcrumb line with the seat chips and `done`, and the counts (`N waiting on you`, `N rows`, `N running`) as their own quiet line under it | the route and the snapshot |
| The switch | `window`, `full board`, `archived` | the page's own state |
| Waiting on you | the rows whose wait is on the person - a look, or one of the four asks - each card leading with the wait's ask, with the row's links and a look's artifact beside it; the verify gate (`approve`, a words box, `send back`), or the ask's own door: the option pills, a free-text answer, the four disposals, or the lesson box; a root whose children are not all done draws the count and the send-back instead of `approve` | `waiting_on`, the request's kind and options, the wait's own start for the age, the row's links, and the row's `rollup` |
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
| `waiting on you` | warn | an ask on the person, or a look park on a row minted `verify: user` |
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
you** when the wait is on the person, the strip's own controls, its **History**
(the row's own transitions, the newest twenty, with `showing the last N of
M` when the tail is capped), and the way into the owner's session. The
button, the scrim and Escape all close it, and the card has the keyboard
back when it goes.

**Every control is one command on the socket, carrying this project on it.**
`task_move` moves a row between lanes and lands it at the drop's place,
`task_rank` re-orders it, `task_assign` changes its owner, `task_verdict`
approves or sends back a verify row, and `task_answer` answers an ask with
the value its kind needs - `choice` for a decision, `text` for a
clarification or a teaching, `disposition` for an exception - with the
note in `words` and a teaching's policy note in `lesson`. `task_ack` marks
an ask seen, `task_snooze` defers it (`until` a span such as `1h`, or
`activity`), and `board_seen` stamps the last look as the reader leaves
the window. `task_create` files a row, `task_reopen` strikes a lead's
attestation, `task_reserve` reserves or releases the pick, `task_restore`
brings a closed row back, and `task_detail` asks for one row whole. A
refused edit says so on the service line rather than silently reverting.

## The states the page can be in

- **A window**, the ordinary case.
- **Before the first frame**: a route can be addressed before the snapshot
  lands; the page says it is reading, and a subscription the server turns
  down says so in its own words instead of reading forever.
- **Unknown**: a project the snapshot does not carry draws its name and one
  line saying this forge does not hold it, with no regions and no create line.
- **Empty**: a project with no live rows draws the window's all-clear as a
  claim with its evidence - when the board was checked, how many agents
  ran, what was sampled and the delta since the last look - and its four
  regions say what they hold none of; the full board draws its three empty
  lanes and the create line. A payload from a server older than the
  attention fields says `nothing is waiting on you` plainly rather than
  inventing evidence.
- **The archive reading, refused, or empty**: the toggle says which.

## Motion

A card arrives, moves and leaves as a move rather than a pop: a new ask
flies in, a card re-ordered or answered slides to its new place, and a
resolved card fades out. A reader whose system asks for reduced motion gets
the position change with no animation. The same motion runs on the desktop
app, the web client and Android, which draw this one page.

## The two widths

Checked at **1600** and at **430**, against the drawing beside it. The window's
need cards are a grid of `minmax(330px, 1fr)` columns, so 1600 sets four
371px asks side by side (373 with an overlay scrollbar), a medium window sets
two, and a phone stacks them;
the lanes are `minmax(260px, 1fr)` columns, so a phone stacks them too, and
neither scrolls sideways. A phone's top bar keeps the count that asks something of the reader
and drops the row totals. Every control on the window takes a 44px target
under `@media (pointer: coarse)` - the full board's owner and reference links
are the pre-existing exception, and nothing here claims them - and the drag is
a pointer gesture, so it is the same one under a finger.