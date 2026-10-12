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
itself - an address read back from the URL carries neither (the read decides
`reviewable` again where it opens, and a deep-linked viewer has no parked
call to release). The page draws no header: the tab names the file, and the
document is the whole of what is on screen.

A document the agent flagged **reviewable** draws as its rendered blocks and
can carry comments; everything else opens read-only, with no comment
affordance anywhere.

## What it draws

| Region                | Shows                                                                                                                                                                                                                                                       | Read from                                                                                                                    |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| The ask               | the dock's card: `document hand-off`, the resolved path, **Open** and **Not now**                                                                                                                                                                           | the `document_hand_off_pending` update (and the seat's `pending_asks` read, for a client that attached while the ask waited) |
| The document          | markdown rendered by the chat's own renderer; plain text as text; a PDF in the surface's own viewer; any other kind named as itself                                                                                                                         | the `document_read` answer: `document_kind` and the bytes                                                                    |
| A reviewable document | its top-level blocks, each one a comment target: hover (or focus) marks the block, a press opens the composer under it                                                                                                                                      | the answer's `threads` and the page's own drafts                                                                             |
| Outdated comments     | written against an earlier version of this document: their captured lines kept, in their own section - nothing re-anchors them                                                                                                                              | each thread's anchor `content_hash` against the answer's                                                                     |
| The done              | the one door, in every state: `done (n)` files the drafts as one round, pushes the review-ready line into the chat, releases the parked hand-off with what was filed, and returns to the session; a refusal draws the core's own words and keeps the drafts | `submit_review`'s reply, then `answer_document_hand_off`                                                                     |
| The chat line         | `Review #N ready on the document - address it via the review MCP (review__list, with the scope doc:<path>)`, the terminal's own nudge                                                                                                                       | the page's own dispatch after the finish                                                                                     |
| A refusal             | the server's sentence as its own line - missing, unreadable, over the ceiling, outside the session's tree, a text kind that is not UTF-8                                                                                                                    | the `error` naming `document_read`                                                                                           |
| Opening               | `Opening <path>...` while the read is out                                                                                                                                                                                                                   | the page's own state                                                                                                         |
| Empty                 | its own line, before any kind is drawn                                                                                                                                                                                                                      | an answer with zero bytes                                                                                                    |

## The rules

**The hand-off is an ask, and the answer is the finish, not the open.** The
raise lands as the dock's card; **Not now** answers the parked call - the
agent is told the reader declined and moves on - while **Open** is the
client's own route and answers nothing. The call releases on the viewer's
**done**, carrying the review the finish filed, so the agent hears what the
reading produced rather than merely that the document opened. A client that
goes away ends the call abandoned, and a forge with no client attached never
parks at all: the tool answers that nothing can open.

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
address read back from the URL re-decides `reviewable` false, so the viewer
never asks - the sentence reaches a reader as the tool's answer and as a raw
read's error, never as a drawn line, because the kind is the path's
extension and a `.pdf` path is refused at the hand-off itself. A
PDF rides the platform's own viewer: WKWebView renders one natively (the
desktop app; measured), every browser does (Chromium measured), and the
Android WebView is the one surface with no PDF viewer of its own, where
pdf.js draws the pages from the same bytes the read answered with. Safari
is the same WebKit engine that was measured; Firefox ships its own pdf.js.

**A comment anchors to a block's own source lines.** A reviewable document
draws as its rendered blocks - each block carries the source span it came
from, and the comment anchors to the block's first line with the span's
lines captured as its context. The anchor carries the WHOLE document's hash -
the same FNV-1a a code thread hashes a line with, over the document's bytes -
so a comment written against this exact content sits at its block, and one
written against another version draws outdated at once, with its captured
lines kept. Nothing re-anchors a document automatically: a code review
re-finds a moved line by its content, and a document has no diff to move
within. A plain-text document has no blocks, so its rows are its own lines,
drawn as the text they are.

**The drafts outlive the viewer.** Unsent comments are held per document,
module-wide, so a second hand-off that raises another document does not take
the first's unsent comments with it - they wait in their own scope until
their done files them.

**The done is the review-left event and the hand-off's answer.** `done (n)`
submits the drafted threads as one round through the same command a code
review uses, under the document's scope (`doc:<path>`), stamped with the
asking seat as the origin - and then answers the parked call with the review
that landed. **The terminal's own drop is the model**: the same one line it
pushes into the session, `Review #N ready on the document - address it via
the review MCP`, goes into the chat here, with the document's scope named -
so the reader sees the drop in the conversation and the agent hears it. Then the
page returns to the session the reader came through: the done is the one
door, and the chat line is the confirmation. A refusal files nothing and
answers nothing: the call stays parked, the drafts stay where they were, and
the refusal's words are what the page draws.

## The states the page can be in

- **The ask waiting**: the dock's card, ahead of the other prompts the way
  the seat's own read orders them; a resolution from another view clears it.
  The seat's own mark is the ask's too - the rail reads it as needs-you
  ("a hand-off is waiting"), and the seat's activity mark is Attention
  while it waits, exactly like a permission or a question.
- **Markdown**: rendered with the chat's renderer - raw HTML never reaches
  the page - and the same `.prose` rules the conversation draws with, on
  the sheet below.
- **The sheet**: the document rests on the app like a page on a desk - one
  panel at a comfortable measure, a hairline over every section, the
  reading scale of the drawing beside this page. The space around it is the
  desk, and the PDF state is the one exception (the platform's viewer takes
  the window).
- **A reviewable markdown or text document**: the rendered blocks instead,
  every block a focusable comment target, with its comments placed under
  their blocks and its drafts under theirs.
- **A comment being written**: the composer opens under the block, with its
  own comment and cancel pair; a written comment draws as `drafted` until
  the done files it.
- **A refused done**: the core's own words beside the button, and the drafts
  stay exactly where they were - a refusal that cleared them would take the
  reviewer's words with it.
- **Done**: `done - review left (R{n})`, and the agent has it.
- **Comments written against an earlier version**: their own section at the
  document's end, each with the captured lines it was written beside.
- **Plain text**: a wrapping `pre`, decoded once; markup in it stays text.
- **A PDF**: the platform's embed over a blob of the bytes, sized to the
  page. On the phone, pdf.js renders each page to a canvas at the column's
  width; a failure in that render draws its own line rather than an empty
  page.
- **Any other kind**: named as itself - `This is a .rs document; the viewer
draws markdown, PDFs and text.` - and a name with no extension says that
  instead.
- **No bytes**: `The document is empty.`, before any kind is drawn.
- **A refusal**: each its own line, straight from the server's sentence -
  and the one door still draws: a refused read returns like any other
  state, answering a parked hand-off on the way out.
- **A read that could not be sent**: the connection is down, in the page's
  own words - a silence that reads as still loading is worse.
- **A connection that dropped mid-read**: the reopen asks again rather than
  leaving the page opening for good.

## The pieces it takes from the pages beside it

- The `.wrap` column and the shared `.pending` line, from every page.
- `Prose`, `renderProse` and `renderProseBlocks` are the chat's own: one
  markdown upstream (`markdown-it`, raw HTML off), one set of `.prose`
  rules, so a document and a quote from one render alike - and the blocks a
  comment anchors to are rendered by the same instance the body is.
- The comment path is the review page's own: `upsertCommand`,
  `submitCommand` and `statusCommand` from `review/write`, and `authorName`
  from `review/view` - one writer, one vocabulary, so a doc comment and a
  code comment are one kind of thing on the wire.
- The ask's card is the dock's own machinery: the same `pending_asks` read,
  the same parked/resolved pair, and the same command-bus answer a browser
  hand-off rides.
- The call's own chat row draws under the **documents** family - the name's
  first segment is the family, so `mcp__forge__documents__hand_off` gets
  its glyph and its row line like every other forge tool.
- The page's own classes are `doc-*` under a `.doc` scope - a new component
  takes its own stem rather than reusing a bare name the sheet already
  styles.

## The widths

**The sheet** (Ved, the look, chosen over the full-bleed page): the
document sits on the app as a page on a desk - one panel capped at 940px,
centred, and the space around it is the desk rather than emptiness. The
reading scale is the drawing's: headings at 30/19/16, body at 15/1.72, a
hairline above every section. The PDF is the one full-bleed state: the
platform's viewer takes the window down to the done row. At 430 the panel
takes the page's padding, long unbroken tokens wrap (`anywhere`) - the sheet
has no sideways scroller and this page brings none.

## Touch

Every control the page carries takes the 44px target under
`@media (pointer: coarse)`: a rendered block (the comment target itself),
the composer's comment and cancel pair, and the done button. Nothing lives
on hover alone - the block's own press opens the composer, and the keyboard
reaches it with a focus and Enter - and the read-only states carry only the
one done (the platform PDF viewer's own chrome is the viewer's).
