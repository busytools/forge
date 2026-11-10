# A document

The viewer a handed document opens on: markdown, a PDF or plain text, drawn
in the client's own viewer rather than in the conversation. An agent hands a
document with `documents__hand_off` and the hand-off arrives as an **ask** in
the composer's dock - the document's resolved path, and two verbs, **Open**
and **Not now**. Open lands on the viewer's own address; a link back to the
document - a review thread's deep link, an address kept - reopens it exactly
where it was. The drawing it is held against is
[web-document.html](./web-document.html), beside this page.

It lives at `/doc/{org}/{project}/{label}/{path...}`, with the document's
resolved absolute path as the trailing segments: **the document IS the
address**, so a hand-off and a deep link spell one. `reviewable` and the
parked call's id ride the ask's Open into the route rather than the address
itself - an address read back from the URL carries neither, and a
deep-linked viewer has no parked call to release. Neither changes what the
page draws: the read answers whether the DOCUMENT is reviewable, so a cold
open draws the same body a hand-off's Open does, and the parked call is the
only thing the hand-off adds. The page draws no header: the tab names the
file, and the document is the whole of what is on screen.

A readable markdown or text document draws as its rendered targets and can
carry comments, whoever opened it. A PDF and any other kind open read-only,
with no comment affordance anywhere.

## What it draws

| Region                | Shows                                                                                                                                                                                                                                                       | Read from                                                                                                                    |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| The ask               | the dock's card: `document hand-off`, the resolved path, **Open** and **Not now**                                                                                                                                                                           | the `document_hand_off_pending` update (and the seat's `pending_asks` read, for a client that attached while the ask waited) |
| The document          | markdown rendered by the chat's own renderer; plain text as text; a PDF in the surface's own viewer; any other kind named as itself                                                                                                                         | the `document_read` answer: `document_kind` and the bytes                                                                    |
| A readable markdown or text document | its rendered targets - a paragraph or heading, one item of a list, one paragraph of a quote - each one a comment target with a `+` on its own line revealed on hover or focus                                                    | the answer's `threads` and the page's own drafts                                                                             |
| A comment on a line   | the target's `+` is the target: hover or focus draws it, and a press opens the composer anchored at that line, the line first in its captured context                                                                                                       | the target's own source line                                                                                                 |
| A comment on a section | a press on the target (outside its `+`) opens the composer anchored at the target's first line, its whole span captured                                                                                                                                     | the target's own source span                                                                                                 |
| Threads               | every turn the thread carries - the reader's words and the agent's reply beside them - drawn under the target their placed line lives in; a draft-held echo draws neither, its draft row standing in | each thread's anchor and the answer's `resolution` |
| A moved thread        | the thread at the line its first captured line was re-found on, with `moved from N` naming where it was written                                                                                                                                            | the landing's `line` and `moved_from`                                                                                        |
| An outdated thread    | the thread at its captured line, marked `outdated` and drawn inline - never collected below the body                                                                                                                                                        | the landing's `outdated` state                                                                                               |
| Could not be placed   | only a thread whose captured line is past this version's last line: its captured lines, the thread's turns, and the sentence saying why                                                                                                                     | the anchor's line against the document's line count                                                                          |
| Editing and deleting  | `edit` on a user turn - filed or migrated with its thread - opens the page's own box over its words, mic and all, and `save` rewrites it; `delete` takes the thread or the draft away, asking first. Either way the document is re-read, so the card draws what the store now holds | `upsert_review_thread` and `delete_review_thread`; the reply, then the `document_read` re-ask                                                                 |
| Resolved              | every open thread carries `resolve` beside its `delete`, writing the same status command the review page rides; the agent's reply never resolves. A resolved comment collapses to its marker line like every other surface's, opened by a press, where `reopen` and `collapse` sit | the thread's status, and the page's own opened set                                                                          |
| The answer            | **approve** and **send feedback**, with one feedback box above them. Approve is the one signal the agent may proceed on; send feedback needs words and sends it back to revise and hand off again, and the row says so under the box. The box's words are the answer's note and the review's description; either answer files the drafts as one round and answers the parked call | `submit_review`'s reply, then `answer_document_hand_off` with the chosen ending, the review id and the note                    |
| The done              | a deep link has no call to answer, so it keeps one `done (n)` that files the drafts, pushes the review-ready line and returns                                                                                                                               | `submit_review`'s reply                                                                                                      |
| The chat line         | `Review #N ready on the document - scope doc:<path>. Read it back under that scope, or hand the scope to your lead if you do not have the review tools`, the page's own nudge                                                                               | the page's own dispatch after the round is filed                                                                             |
| A refusal             | the server's sentence as its own line - missing, unreadable, over the ceiling, outside the session's tree, a text kind that is not UTF-8 - and the two answers still draw for a parked call                                                                | the `error` naming `document_read`                                                                                           |
| Opening               | `Opening <path>...` while the read is out                                                                                                                                                                                                                   | the page's own state                                                                                                         |
| Empty                 | its own line, before any kind is drawn                                                                                                                                                                                                                      | an answer with zero bytes                                                                                                    |

## The rules

**The hand-off is an ask, and the answer is the reader's decision, not the
open.** The raise lands as the dock's card; **Not now** answers the parked
call - the agent is told the reader declined and moves on - while **Open** is
the client's own route and answers nothing. The call releases on one of two
explicit outcomes: **approve** or **send feedback**. Neither is a bare
done: **a reader who chooses neither leaves the call parked**, and nothing a
press on the body does releases it. Both carry the one feedback box's words,
which reach the agent verbatim with the answer, and the review the finish
filed; an empty press on send feedback draws the requirement and sends
nothing rather than disabling the button.
A detached client leaves the call parked, and the ask is re-readable when a
client returns; only the waiter's own drop abandons it. A forge with no
client attached never parks at all: the tool answers that nothing can open.

**The loop is the shipped text's.** The tool description and the session
conduct block both state it: feedback means revise the document and hand it
off again, and the agent keeps handing it off until the reader approves;
approval is the one signal to proceed, and an agent never proceeds on
feedback. The page needs no board link for it - the answer itself drives the
loop, and the agent raises the next hand-off after every feedback.

**The answer is matched by seat AND document.** A client holding two seats
can have two viewers' reads in flight; the answer names both back, and a
page that matched only on the path would draw one seat's document in the
other's viewer.

**Every refusal is its own sentence.** The server is the one place that
knows why a read did not happen, so the client draws its `why` rather than
composing a reason of its own - including the connection being down, which
is the page's own sentence because nothing on the wire could carry it. A
text kind that is not UTF-8 is refused at the READ: decoding it here would
draw replacement characters as if they were the document.

**A PDF opens read-only, and the reviewable flag on one is refused with the
reason.** The refusal is the read's own and happens before any raise: the
hand-off tool answers with the sentence and nothing asks the reader, and an
address read back from the URL carries no hand-off, so the viewer never asks
for one reviewable - the sentence reaches a reader as the tool's answer and
as a raw read's error, never as a drawn line, because the kind is the path's
extension and a `.pdf` path is refused at the hand-off itself. A
PDF rides the platform's own viewer: WKWebView renders one natively (the
desktop app; measured), every browser does (Chromium measured), and the
Android WebView is the one surface with no PDF viewer of its own, where
pdf.js draws the pages from the same bytes the read answered with. Safari
is the same WebKit engine that was measured; Firefox ships its own pdf.js.

**A comment anchors to a source line, or to a target's span.** A reviewable
document draws as its rendered targets: a paragraph or heading, one item of a
list, one paragraph of a quote (a table stays one target). Each carries the
source span it came from and owns the blank lines up to the next target.
There is no always-visible numbering: the target is the section target, and a
`+` on its own line is the line target, drawn on hover or focus (and drawn
always on a finger-driven surface). A press on the target anchors at its
first line with the span captured as its context; a press on the `+` anchors
at that exact line with the line FIRST in its captured context, capped at 12
lines - the line and the ones after it. That first captured line is the
thread's identity across a revision. The anchor also carries the line's own
hash, the same FNV-1a a code anchor hashes its line with. The highlight is a
neutral ground, never an accent bar, and the `+` is a plain mark in the
margin rather than a chip. A plain-text document has no blocks, so its rows
are its own lines, drawn as the text they are, each with its own `+`.

**A thread re-anchors across a revision the way a code thread does.** The
server resolves each doc thread against the document it is read with: the
first captured line's content is looked for - in place at the recorded line,
otherwise one unique match moves it - and the landing crosses on the read.
A moved thread draws at its new line with `moved from N`; a thread the
revision left nowhere draws at its captured line, marked `outdated` and
still inline, because collecting it below the document would hide what it
was about. Only a thread whose captured line is past this version's last
line collects at the end, with the reason said in words. A thread the
landing MOVED is not drift and is not aged in the store; one with no landing
is, so the review record reads what the reader sees - except a draft-held
echo: it is not drawn and not aged while its draft row stands in, and a
draft whose line no row holds draws only as its echo.

**A comment's words render as markdown, through the chat's own renderer** -
the same `Prose` the review page's turns draw with, the author's lines kept:
a comment carrying backticks or an emphasis reads here the way it reads in
the conversation. Every turn of a thread is drawn with `ThreadTurn`, so the
round chip and the edit act are the review page's own.

**The drafts outlive the viewer.** Unsent comments are held per document,
module-wide, so a second hand-off that raises another document does not take
the first's unsent comments with it - they wait in their own scope until an
answer files them.

**The comment box and the feedback box dictate, the way the composer's
does.** The comment box wears the composer's own mic control, and the
feedback box does too: a press starts a take, a hold opens what dictation is
set to, and the take's bar takes the box's own line while it runs. The words
land at the end of the box that was open when the take settled, read off the
seat's record. The comment composer is the exception to a closed box: words
submitted before the reader closed it reopen the box at the row and line
that began the take, where the reader sees them. The edit and feedback boxes
no longer draw once closed, so their words are dropped rather than written
where the reader is not, and a notice about a take this page never watched
is not placed at all. The push-to-talk key is honoured while a box is open,
the same press and release the composer's box reads.

**One feedback box is the description and the note.** A single box sits
above the answers when a hand-off is parked or there is a review to file.
Its words ride the submit as the round's summary and the
`answer_document_hand_off` as the note, so an agent without the review
family still learns what the reader said. Approve may go with it empty;
send feedback requires words, and the row says so in a hint under the box -
an empty press turns that hint into the warning and keeps the button in the
tab order.

**The done is the review-left event on a deep link.** `done (n)` submits the
drafted threads as one round through the same command a code review uses,
under the document's scope (`doc:<path>`), stamped with the asking seat as
the origin - and then returns to the session, with no call to answer. **The
terminal's own drop is the model** for the line it leaves: the same one-line
drop, `Review #N ready on the document`, goes into the chat, with the
document's scope named - so the reader sees the drop in the conversation and
the agent hears it. The line names no review tool and promises no answer
that follows. A refusal files nothing and answers nothing: the call stays
parked, the drafts stay where they were, and the refusal's words are what
the page draws.

## The states the page can be in

- **The ask waiting**: the dock's card, ahead of the other prompts the way
  the seat's own read orders them; a resolution from another view clears it.
  The seat's own mark is the ask's too - the rail reads it as needs-you
  ("a document hand-off is waiting"), and the seat's activity mark is
  Attention while it waits, exactly like a permission or a question.
- **Markdown**: rendered with the chat's renderer - raw HTML never reaches
  the page - as its rendered targets, on the sheet below.
- **The sheet**: the document rests on the app like a page on a desk - one
  wide centred panel that uses the window's width, a hairline over every
  section, the reading scale the drawing beside this page carries. The page
  background stays the window's width. A PDF is the exception: it takes the
  window.
- **A readable markdown or text document**: its rendered targets, each one a
  comment target with a `+` on its own line shown on hover or focus, its
  comments placed under their targets and its drafts under theirs, a
  draft-held echo drawn by its draft row instead - a cold open included,
  which draws the same body a hand-off's Open does.
- **A comment being written**: the composer opens under the target, with its
  own comment and cancel pair and its mic; a written comment draws as
  `drafted` until an answer files it.
- **A take running**: the take's bar takes the box's own line - the
  composer's own reading, `ready`, `recording` and `transcribing` - and the
  words land at the end of the box when the take settles.
- **An answer sending**: the pressed button says `approving...` or
  `sending...` and both disable while the round files and the call is
  answered.
- **A refused answer**: the core's own words beside the answer row and both
  buttons live again. A refused FILING keeps the drafts exactly where they
  were; a refused answer after the filing keeps the review it filed and its
  id, so the next press still names it.
- **Approved**: `approved - review left (R{n}) - the agent has it.`
- **Sent back**: `sent back - review left (R{n}) - the agent has it.`
- **Done (a deep link)**: `done - review left (R{n}).`
- **A moved thread**: drawn at its new line with `moved from N` beside its
  mark, the agent's reply still in the thread.
- **An outdated thread**: drawn at its captured line, marked `outdated`, its
  captured lines kept.
- **A thread with nowhere to draw**: its own section at the document's end,
  with the captured lines and the sentence naming the line it was written at
  and how many this version has.
- **Plain text**: wrapping rows, decoded once and each one a comment target
  with its own `+`; markup in it stays text.
- **A PDF**: the platform's embed over a blob of the bytes, sized to the
  page. On the phone, pdf.js renders each page to a canvas at the column's
  width; a failure in that render draws its own line rather than an empty
  page.
- **Any other kind**: named as itself - `This is a .rs document; the viewer
  draws markdown, PDFs and text.` - and a name with no extension says that
  instead.
- **No bytes**: `The document is empty.`, before any kind is drawn.
- **A refusal**: each its own line, straight from the server's sentence -
  and the answer row still draws for a parked call, so a refused read
  returns like any other state.
- **A read that could not be sent**: the connection is down, in the page's
  own words - a silence that reads as still loading is worse.
- **A connection that dropped mid-read**: the reopen asks again rather than
  leaving the page opening for good.

## The pieces it takes from the pages beside it

- The `.wrap` column and the shared `.pending` line, from every page.
- `Prose`, `renderProse` and `renderProseTargets` are the chat's own: one
  markdown upstream (`markdown-it`, raw HTML off), one set of `.prose`
  rules, so a document and a quote from one render alike - and the targets a
  comment anchors to are rendered by the same instance the body is.
- The comment path is the review page's own: `upsertCommand`,
  `submitCommand` and `statusCommand` from `review/write`, and `ThreadTurn`
  for every turn - one writer, one vocabulary, so a doc comment and a code
  comment are one kind of thing on the wire.
- The dictation pieces are the composer's own: the `Dictation` take, the
  `DictateControl` mic, the `TakeCard` bar, the `DictationPanel` and the
  `Landing` memory, all under `composer/` - one microphone, one take, one
  reading of what a take is doing, shared with every box that takes typing.
- The ask's card is the dock's own machinery: the same `pending_asks` read,
  the same parked/resolved pair, and the same command-bus answer a browser
  hand-off rides.
- The call's own chat row draws under the **documents** family - the name's
  first segment is the family, so `mcp__forge__documents__hand_off` gets
  its glyph and its row line like every other forge tool.
- The page's own classes are `doc-*` under a `.doc-page` scope - a new component
  takes its own stem rather than reusing a bare name the sheet already
  styles.

## The widths

**The sheet is a wide centred page and the page background is the window's.**
`.doc-body` is capped at 1400px and centred with `margin-inline: auto`, so
the document uses the space rather than reading as a narrow column with a
desk of empty room beside it; the answer row is capped to the same width.
**Prose runs the sheet's whole width** - a paragraph, a list and a heading
fill it - and code fences, tables and long unbroken tokens take it too. The
chat's own table rule stacks a table when its column is narrower than 760px;
above that a table draws as a table, and the phone's sheet keeps the stack.
The document route is its own scroller and wears the theme's one scrollbar
skin, its corner included, so the platform's white block no longer shows at
the bottom-right. At 430 the sheet takes the page's padding, the table stacks
at its container width, and long tokens wrap (`anywhere`) - the sheet has no
sideways scroller and this page brings none.

## Touch

Every control the page carries takes the 44px target under
`@media (pointer: coarse)`: a rendered target (the section comment target),
the `+` on its own line (the line comment target), the composer's comment
and cancel pair, the answer buttons and the one done. Nothing lives on hover
alone: the `+` is drawn always on a finger-driven surface, the target's own
press opens the section composer, the `+`'s press comments on
that line, and the keyboard reaches both with a focus and Enter. The
read-only states carry only the answer row (the platform PDF viewer's own
chrome is the viewer's).
