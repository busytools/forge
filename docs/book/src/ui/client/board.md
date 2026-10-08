# Board

One project's task board, opened as a takeover at
`/board/{org}/{project}`: the rows ranked as the lead would dispatch them,
the ones waiting on the reader, and the controls that move them. The fleet
stays the glance; this is the world. Nothing of any other project appears
here. The drawing it is held against is [web-board.html](./web-board.html),
beside this page.

It is reached from two doors: the project's row on the [home](./home.md),
and the tasks strip in a session's chat, which carries `open board` first in
its list. Both open the same page. `back` and `done` return the reader where
they came from, and a deep link with nothing to go back to lands on the
home.

The page draws from the same `home` snapshot the fleet does - a project's
rows ride in that project's entry - so a board open beside the fleet updates
from the same stream, and the 30-second chase sweep's announcement moves the
ages and marks here without a reader doing anything.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Top bar | the project's name, `back`, `done` | the route and the snapshot |
| Seats | one chip per seat this project holds, linking to its session page | `agents` filtered to the project |
| Waiting on you | the rows whose wait is a decision: the verify gate (`approve`, a words box, `send back`) or a worker's question (an answer box) | `waiting_on` with `kind: decision` |
| Rows | the ranked tree, two levels deep: an epic, then its children | `projects[].rows`, ranked by `rank` then creation |
| Create | a subject box, an epic picker and `+ add`, which files the row at the queue's end | `task_create` |

**Every control is one command on the socket, carrying this project on it.**
The page holds no state of its own beyond what is typed: `task_rank` moves a
row, `task_assign` changes its owner, `task_verdict` approves or sends back a
verify row, `task_answer` answers a question, `task_create` files a row. A
refused edit says so on the service line rather than silently reverting, and
every one of them stamps the change as the user's.

## A row

A row is one grid, left to right: the status mark, the subject, the owner,
the worked time against the estimate, the chips, the links, and the controls.
A row in progress displays its `active_form` where it has one, because that
is what the reader wants to know - subjects are what a row is, active forms
are what it is doing. A completed row's subject is struck through. A child
sits indented under its epic, and a parent carries a `done/total` rollup of
its children beside its own chips.

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

## The states the page can be in

- **A board**, the ordinary case.
- **Waiting on the reader**: the section draws only when at least one row's
  wait is a decision. Its absence is not a state - an empty frame above the
  rows would read as a board with nothing for the reader, which is a
  different claim from the section not being there.
- **Empty**: a project with no live rows draws one line naming the two moves
  it has - cut an epic in the create line, or ask the lead - rather than a
  bare list.
- **Before the first frame**: a route can be addressed before the snapshot
  lands, and a project the snapshot does not carry reads as an empty board
  under its name. The page arrives with the connection rather than drawing a
  second loading state of its own.
- **A set of seats**: the seat chips draw only when the project holds one; a
  project with nothing running draws none.

## The two widths

Checked at **1600** and at **430**, against the drawing beside it. The
board's own collapse is at 900: the row keeps three columns - mark, subject,
controls - and the owner, the worked time, the chips and the links drop to
lines under the subject. Every control a finger reaches takes a 44px target
under `@media (pointer: coarse)`, and nothing overflows horizontally at
either width.