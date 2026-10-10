# Board

One project's task board, opened as a takeover at
`/board/{org}/{project}`: a slim bar, the rows waiting on the reader, the
project's cards in lanes by state, and the line that cuts a new row. The
fleet stays the glance; this is the world. Nothing of any other project
appears here. The drawing it is held against is
[web-board.html](./web-board.html), beside this page.

It is reached from two doors: `open board` on the project's row on the
[home](./home.md), and the tasks strip in a session's chat, which carries
`open board` first in its list. Both open the same page. `back` and `done`
return the reader where they came from, and a deep link with nothing to go
back to lands on the home.

The page draws from the same `home` snapshot the fleet does - a project's
rows ride in that project's entry - so the board is live off the same
subscription: a seat that claims a row, a row that is cut, a move made in
another view all arrive as an update, and the page re-reads without the
reader doing anything.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Top bar | `back`, the project's name with its own read (`N waiting on you`, `N rows`, `N running`), `done` | the route and the snapshot |
| Seats | one chip per seat this project holds, linking to its session page; a card dropped on one assigns it | `agents` filtered to the project |
| Waiting on you | the rows whose wait is a decision: the verify gate (`approve`, a words box, `send back`) or a worker's question (an answer box). A root whose children are not all done draws the count and the send-back instead of `approve`, because a row closes only when its children do | `waiting_on` with `kind: decision`, and the row's `rollup` |
| Lanes | every card, in its state's lane | `projects[].rows` |
| Create | a subject box, an epic picker and `+ add`, which files the row at the queue's end | `task_create` |

## The lanes

Six lanes, always drawn in this order, because a missing lane would read as
a missing state: **In progress**, **Waiting**, **Ready** (pending),
**Completed**, **Failed**, **Canceled**. A lane is a name, its count and a
rule, with its cards under it - no box around the column - and the lanes wrap
to a second row rather than scroll sideways.

### A card

Three lines, top to bottom:

- the mark, then the subject. A row in progress displays its `active_form`
  where it has one, with the subject under it, because that is what the
  reader wants to know - subjects are what a row is, active forms are what it
  is doing. A completed row's subject is struck through.
- the owner (their initial in a disc, linking to their session page, or
  `unclaimed`), the epic it belongs under where it has one, its links, and
  its chips - with a `done/total` rollup beside them on a parent.
- worked time against the estimate with the measure under it, and how long
  since the row last moved (`wrote 12s ago`, breathing while the row is
  running and recently wrote).

The mark carries shape as well as colour, so the board reads without colour:

| Status | Mark | The state |
|---|---|---|
| Pending | a ring | queued, not started |
| In progress | a filled accent dot | a seat is on it |
| Waiting | a warning square | a wait, a question or the verify gate |
| Completed | a filled green dot | done |
| Failed | a filled red dot | it ended in failure |
| Canceled | a dim bar | withdrawn, kept for the record |

The chips are the server's derived marks, named in words:

| Chip | Tone | Means |
|---|---|---|
| `waiting on you` | warn | a decision wait, or the task's verify gate |
| `on <row>` | warn | waiting on another row's completion |
| `waiting on a resource` | warn | waiting on something outside the board |
| `waiting <3h>` | bad | the wait has outlived the threshold |
| `overdue` | bad | worked time has crossed the estimate |
| `no movement <5h>` | dim | untouched, and nothing is waiting |
| `owner gone` | bad | its owner is no longer a live seat |
| `in review` | blue | completed with the verify gate up |
| `to close` | blue | a root whose children are all done, waiting for its close |
| `attempt N` | dim | sent back with words N times |

The links come off the row's own list: a URL follows in a new tab under its
label, and a path or a branch draws as text rather than a dead anchor.

## Moving a card

The drag is the board's interaction, and what the card lands on decides the
command: another lane is a move (`task_move`), a position inside its own lane
is a re-rank by drop position (`task_rank`), and a seat chip is an assignment
(`task_assign`). A press that never passes six pixels is a click, not a drag.
A dropped card lands where it was put at once and the wire's own snapshot
reconciles it a moment later; it animates into place unless the reader has
asked for reduced motion, and the card under the pointer tilts and lifts.

The keyboard has the same power without a row of buttons: the arrow keys on
a focused card re-order it within its lane, and the owner picker on the card
is a control like any other.

**Every control is one command on the socket, carrying this project on it.**
`task_move` moves a row between lanes, `task_rank` re-orders it,
`task_assign` changes its owner, `task_verdict` approves or sends back a
verify row, `task_answer` answers a question, `task_create` files a row. A
refused edit says so on the service line rather than silently reverting, and
every one of them stamps the change as the user's.

## The states the page can be in

- **A board**, the ordinary case.
- **Waiting on the reader**: the strip draws only when at least one row's
  wait is a decision. Its absence is not a state - an empty frame above the
  cards would read as a board with nothing for the reader, which is a
  different claim from the strip not being there.
- **Empty**: a project with no live rows draws one line naming its two moves
  - cut a row in the create line, or tell the lead what is next - with the
  six lanes still drawn under it.
- **Before the first frame**: a route can be addressed before the snapshot
  lands. The page arrives with the connection rather than drawing a second
  loading state of its own, and a subscription the server turns down says so
  in its own words instead of reading forever.
- **Unknown**: a project the snapshot does not carry draws its name and one
  line saying this forge does not hold it, with no lanes and no create line -
  a row filed there would name a project `forge.toml` has no entry for.
- **A set of seats**: the seat chips draw only when the project holds one; a
  project with nothing running draws none.

## The two widths

Checked at **1600** and at **430**, against the drawing beside it. The lanes
are a grid of `minmax(260px, 1fr)` columns, so a wide window fits them side
by side, a phone stacks them, and neither scrolls sideways. Every control a
finger reaches takes a 44px target under `@media (pointer: coarse)`, and the
drag is a pointer gesture, so it is the same one under a finger.
