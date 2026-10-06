# Dictation models

The page that shows which dictation models forge runs, what the runtime's
own catalogue publishes, and what the feed proposes adopting. It is drawn
from one snapshot of the `dictate_models` subject - the pins, the
catalogue check, the proposals and every row of the feed - and redrawn from
the update stream after it. The drawing it is held against is
[web-dictate-models.html](./web-dictate-models.html), beside this page.

It lives at `/models`, which is its own address rather than a session's: the
models belong to the forge, not to a seat, and the subject carries no slot.
The header's way back is the home.

This page is a client-only surface: the terminal draws no catalogue. The
read and the check command are the server's (`Subject::DictateModels`,
`Command::DictateCatalogueCheck`), and this page is the first view of
either.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Header | the brand mark, `forge`, the page's name, and the way back to the home | `ClientSettings.mark` from the greeting; the route |
| In use | one row per pinned model: its role, its file, the facts the pin declares - size, quant, parameters, digest, licence - and the feed's own measurement when it has one, with the live state as a chip | `in_use` |
| Updates | what a check reads, the check's own line - up to date, an update, checking, unreachable, or a state this client cannot read - and one line per proposal: its facts, the rule it was admitted on, and what taking it means | `check`, `updates` |
| Find a model | a box that filters the feed's rows as it is typed, with the feed's families and its fastest rows offered before a name is known: the variant, the quant a machine would run, the measured speed and error, the licence, what it transcribes. Each row links to the entry's own document | `rows` |
| Benchmark | the section and what the run will do; the run itself is a separate piece of work | - |

**A pin is drawn from the pin.** The first fact line comes off the pinned
`ModelSpec` - the size, quant, parameters, digest and licence the loader
verifies before a load - and never off the feed, so a model the feed does
not carry still draws whole. The second line is the feed's: its runtime
measurement and its languages. A pin with no join says `not in the feed`
rather than borrowing a measurement.

**The check's line is the feed's freshness and its proposal at once.** A
fresh check with nothing to propose reads `up to date`; the same check with
a proposal reads `update available`, because they are one state of one
thing. The line states when the check ran - as a local time, from the
server's RFC 3339 stamp - and the release the feed stood at when it
answered. An `unreachable` check carries the server's own error text, and
the rows the last fetch left stand. **The note under the line says what a
check is**, because the control does not: it reads the catalogue the
runtime publishes - every variant with its sizes, licences and the speeds
and error rates its maintainers measured - and compares that with the two
models pinned here. Nothing is measured on this machine.

**An update line says what it is, why it was picked, and what taking it
means.** It names the model it would replace, draws the comparison the
server admitted it on - the candidate's speed and error against the model in
use - states the rule in words (it beats the model in use on both of the
feed's own measurements, and its licence allows forge to ship it), and says
what taking it is: pinning it here and opening a pull request. The bench
that re-checks a candidate on this machine's own recordings is the piece
that is not built, and the line says so rather than leaving a reader to look
for a control that is not there.

**The search is the client's, and it filters as the box is typed.** The
whole feed arrives with the read, so there is no button to press and nothing
is asked of the server. The rows are the feed's own figures; the note says
they are measured on an m4 max, not on this machine. **Each row is a link to
its catalogue entry** - the feed's own document, in the same tree the server
fetches from - because a list of rows that goes nowhere is what a reader
clicks first, and the marker that says so is drawn at rest rather than
uncovered by the pointer.

**The box is blind on its own, so the page offers what can be searched.**
Before a name is typed it draws the feed's own families as chips - each one
naming a class the feed holds, with its entry count, most-populated first -
and its three fastest measured rows under them. Both set the query, so a
pick is the same mechanism as typing rather than a second one, and every
family name is one `search` matches its own rows by. The feed names no
dates, so "latest" is not a thing this page can say; its own measurement is
the only ranking it carries.

## The states the page can be in

- **The ordinary case**: both pins loaded, a fresh check, nothing to
  propose.
- **An update available**: one line per in-service model worth adopting,
  which can be one or both.
- **Dictation off**: `[dictate] enabled` is unset, so no model is in use,
  none is proposed, and the feed has not been read at all - the server
  loads the catalogue only for an enabled section, so the read answers no
  rows and no check. It is its own state, naming the key that would switch
  it on, and the search draws a note saying the same rather than a box that
  would answer every query with `no entry matches`.
- **Reading**: connected, and the subject's snapshot has not landed. The
  page says it is reading rather than drawing an empty catalogue.
- **Refused**: the server turned the subscription down, in its own words.
  The address is not what is wrong, so the door is not drawn.
- **Checking**: a check is in flight - this page's own click, or boot's -
  and there is no second Check now to press. A second check is refused, and
  a control that reports nothing when pressed reads as broken.
- **Unreachable**: the last check could not reach the feed. The server's
  error is drawn under the line, and the last-known rows stand: a failed
  check costs the freshness line, not the feed.
- **An unknown check state**: a `state` tag this client is older than is
  narrowed once, where the read enters (`wire/models.ts`), into the page's
  own `unknown` - which says so. It never draws as `never`, which would
  claim nothing has fetched on a machine that has.
- **A model mid-load**: the pin's chip carries the state the preflight
  snapshot reports - waiting, verifying, fetching with its fraction,
  loading, loaded, or failed. The page re-reads when the load finishes
  (`dictate_availability`), so chips do not stay on `waiting` until the
  next check.
- **A connection that dropped**: the shell's own line stands above every
  page, and this one keeps what it last read.
- **A search that matches nothing**: the query is named, with the hint that
  a family name is what it matches on.

## The pieces it takes from the pages beside it

- The header, the brand lockup and the `.wrap` column, from the home.
- The band card's ground - the gradient over `--line` - as the row's
  surface, and the `.svc` card's own shape for the check and update lines.
- The empty box (`dictation is off`, and the benchmark section) from the
  home's own empty treatment.
- The chip, at this page's scale, from the dictation panel's.
- The state marks: the four shapes are the shared vocabulary
  (`web.css`), and the two this page needed - the verdict disc and the
  verdict triangle - were added to that vocabulary rather than to the page,
  because the band draws the same two.

## The two widths

The page is checked at **1600** and at **430**, against the drawing beside
it. At the sheet's 760 breakpoint the model row wraps: the role and the chip
take a line of their own, and a candidate row's facts wrap under its name.
Nothing overflows horizontally at either width.

## Touch

Every control takes the 44px target under `@media (pointer: coarse)` - the
shared rule covers a button and an input, and the page adds the two it
cannot reach: the way back to the home, and a candidate row. No affordance
lives on hover alone.
