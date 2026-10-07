# Dictation models

The page that shows which dictation models forge runs, what the runtime's
own catalogue publishes, which models this machine has downloaded, and how
to bring a candidate onto the machine and make it the one dictation runs. It
is drawn from one snapshot of the `dictate_models` subject - the models in
use, the catalogue check, the proposals, every row of the feed, the installed
set and the download/activation state - and redrawn from the update stream
after it. The drawing it is held against is
[web-dictate-models.html](./web-dictate-models.html), beside this page.

It lives at `/models`, which is its own address rather than a session's: the
models belong to the forge, not to a seat, and the subject carries no slot.
The header's way back is the home.

This page is a client-only surface: the terminal draws no catalogue. The
read and the actions are the server's (`Subject::DictateModels`,
`Command::DictateInstall`, `Command::DictateActivate`,
`Command::DictateDeactivate`), and this page is the first view of either.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Header | the brand mark, `forge`, the page's name, and the way back to the home | `ClientSettings.mark` from the greeting; the route |
| The page's state | a refused action in the core's own words, and whichever download or activation is in flight with its progress | `refusal` (an `error` frame); `install`, `activate` |
| In use | one row per model forge runs: its role - **the selector for the updates below** - its file, the facts the spec declares (size, quant, parameters, digest, licence), the feed's own measurement when it has one, the live state as a chip, and a line saying where the model came from | `in_use` |
| Updates | the check's own line - up to date, an update, checking, unreachable, or a state this client cannot read - one line per proposal with the control that takes it, and the comparison table under it | `check`, `updates` |
| Find a model | a box that filters the feed's rows as it is typed: the variant, the quant a machine would run, the measured speed and error, the licence, what it transcribes, and the row's control. Each row links to the entry's own document | `rows` |
| Benchmark | one row per model a bench can run, with the controls that run it and stop it; the read-aloud set's own state, and the record control when there is none; one row per saved result, with its figures, its verdict against the model in use, and its delete | `bench`, `read_aloud`, `results` |

**A model in use is drawn from its spec.** The first fact line comes off the
`ModelSpec` - the size, quant, parameters, digest and licence the loader
checks before a load - and never off the feed, so a model the feed does not
carry still draws whole. The second line is the feed's: its runtime
measurement and its languages. A model with no join says `not in the feed`
rather than borrowing a measurement. The third line says where the model
came from: `compiled default`, `installed here`, or `pinned by [dictate]
<key>` - naming the key, because that key is what has to go for the runtime
to move the role.

**The check's line is the feed's freshness and its proposal at once.** A
fresh check with nothing to propose reads `up to date`; the same check with
a proposal reads `update available`, because they are one state of one
thing. The line states when the check ran - as a local time, from the
server's RFC 3339 stamp - and the release the feed stood at when it
answered. An `unreachable` check carries the server's own error text, and
the rows the last fetch left stand.

**A row's control is the rule the core enforces, drawn.** A variant this
machine does not have offers `install <quant>`; the press fetches the
variant's doc, takes the quant the row draws, and downloads it. An installed
variant that is not the active transcribing model offers `use for
transcribing`; the press loads it while the current model keeps running -
the core builds the new engine first, swaps, and only then drops the old
one. The active model draws as a state (`active`) rather than a control, and
an installed model under a config pin draws `installed` with no activation
control at all: `forge.toml` wins over every runtime pick, so the core
refuses that dispatch, and a control that is always refused reads as
broken. The download stays on a pinned role - pulling candidates down is
still allowed.

**An update line carries its own control**, which is the recommendation made
pressable: the candidate's comparison against the model in use, then the
install or activation the rule above would draw - download when the variant
is not here, then load it, one press, with the completion line when the role
runs it. The bench that scores a candidate on this machine's own recordings
is the section below, not a second control in the line.

**The comparison table is the rule's own working**, one row per model and
one column per axis. Its columns are fixed widths, so the geometry holds
still as the state changes under it - the in-use row's file name, the pick's
control - rather than the table re-laying itself out while it is being read.
The baseline row is the model in use: it draws its licence like every row
does, and the words saying it is what runs sit in the rule column. Every
candidate is read against that baseline - speed and error as `vs` pairs, the
licence as the feed spells it, and the rule's verdict: `recommended`, `also
beats both, but slower`, `slower than this`, `no more accurate`, or a verdict
this client is older than. The pick's control is drawn beside its verdict
rather than in a last column of its own, so a row's action is labelled by the
cell it sits in.

**The role row is the selector.** Pressing a role in use draws that role's
own proposal below - the section follows the first role that has one until a
press, so a role with news is never hidden behind one without. A role the
feed has nothing for says so in words rather than leaving another role's
table standing under its name, and a proposal whose candidate list is empty
says that too. The role is a button rather than the row itself, for the
reason a row's name is the link on the home: the row carries its own
controls, and a control inside a control is not HTML.

**The bench scores a model on this machine's own material, and nothing is
embedded.** A shipped binary carries no audio: a machine with no material
says so, and the way to get material is to record it. Two tiers. The
consensus tier is the takes forge has saved here, the candidate's words read
against the words the model in use recorded beside each one - speed and
agreement, and agreement is a signal rather than an error, because the other
side is another model's output. The read-aloud tier is the one passage
somebody read aloud on purpose: its words are known, so it is the only corpus
that can score term accuracy and word error rate. **The page records that
passage itself**: the section draws the passage and the record control, and a
press opens this client's microphone through the composer's own capture. The
recording is not a take - no session owns it and nothing is transcribed,
because the passage's words are already known - and the card draws its clock,
its frames and its levels while it runs, with stop and save and cancel. Every
recording is KEPT: with one standing, the section draws the recordings
themselves (each one's length, when it was made, and a delete) with a control
to add another, and the passage is not drawn again - a reader who has read it
does not need it under every state. A run names its target, its own progress,
and a stop that discards a partial corpus rather than saving one. Each saved
result draws its figures, what it means against the model in use on the same
corpus, and a delete.

**The search is the client's, and it filters as the box is typed.** The
whole feed arrives with the read, so there is no button to press and nothing
is asked of the server. The rows are the feed's own figures; the note says
they are measured on an m4 max, not on this machine. **Each row is a link to
its catalogue entry** - the feed's own document, in the same tree the server
fetches from - because a list of rows that goes nowhere is what a reader
clicks first, and the marker that says so is drawn at rest rather than
uncovered by the pointer.

**A download is never called verified.** These files publish no digest; the
core checks a download against the feed's own byte length and the engine's
own load, and the page's note says exactly that. Nothing on this page may
print `verified`, `checksum` or a digest the spec did not declare.

## The states the page can be in

- **The ordinary case**: both models loaded, a fresh check, nothing to
  propose.
- **An update available**: one line per in-service model worth adopting,
  which can be one or both, each with the control that takes it.
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
  claim nothing has fetched on a machine that has. The download and
  activation states take the same treatment, and so does a source for a
  model in use this client has no case for.
- **A model mid-load**: the row's chip carries the state the preflight
  snapshot reports - waiting, verifying, fetching with its fraction,
  loading, loaded, or failed. The page re-reads when the load finishes
  (`dictate_availability`), so chips do not stay on `waiting` until the
  next check.
- **A download in flight**: one line under the header naming the file, the
  whole percent and the bytes, with a progress element carrying the same
  figure. Every install and activation control is disabled while it runs -
  the core takes one download at a time - so a second press is not offered
  and then refused.
- **A download that did not finish**: the same line in the failed tone, with
  the file and the core's own reason (a byte-length disagreement carries
  both lengths).
- **An activation in flight**: a line naming the file being loaded and the
  role it will take, saying that dictation keeps running the current model
  until the new one is up.
- **An activation that did not load**: the failed line carries the reason
  and says the current model is still running - the core builds the new
  engine before it swaps, so a failure changes nothing.
- **A refused action**: the core's own words under the header, on an
  `error` frame the connection answered the dispatch with - a second
  download while one runs, or an activation on a role `forge.toml` pins.
- **A pinned role**: its row's source line names the `[dictate]` key, no
  row offers an activation for it, and the download controls stay.
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
- The chip, at this page's scale, from the dictation panel's - as a state
  when it is a span and as an action when it is a button.
- The state marks: the four shapes are the shared vocabulary
  (`web.css`), and the two this page needed - the verdict disc and the
  verdict triangle - were added to that vocabulary rather than to the page,
  because the band draws the same two.
- The `.status` line, for the download and activation states, is the check
  and update lines' own shape; the download's bar is the platform's
  `progress` element, so the value it draws is the value the words beside it
  carry.

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
