# A document

The viewer a handed document opens on: markdown, a PDF or plain text, drawn
in the client's own viewer rather than in the conversation. An agent hands a
document with `hand_off_document`, the viewer opens, and a link back to the
document - a review thread's deep link, an address kept - reopens it exactly
where it was. The drawing it is held against is
[web-document.html](./web-document.html), beside this page.

It lives at `/doc/{org}/{project}/{label}/{path...}`, with the document's
resolved absolute path as the trailing segments: **the document IS the
address**, so a hand-off and a deep link spell one. `reviewable` is the
hand-off's flag rather than the address's - it rides the raise into the
route, and an address read back from the URL re-decides it where it opens
(the read refuses it for a PDF either way). The page draws no header: the
tab names the file, and the document is the whole of what is on screen.

A document the agent flagged **reviewable** draws as its own numbered lines
and can carry comments, which leave like any review; everything else opens
read-only, with no comment affordance anywhere.

## What it draws

| Region                | Shows                                                                                                                                                     | Read from                                                 |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| The document          | markdown rendered by the chat's own `Prose`; plain text as wrapping text; a PDF in the surface's own viewer; any other kind named as itself               | the `document_read` answer: `document_kind` and the bytes |
| A reviewable document | its own lines, numbered from one, each number a target that opens a composer; a comment waits under the line it is about, drafted until a finish files it | the answer's `threads` and the page's own drafts          |
| Outdated comments     | written against an earlier version of this document: their captured lines kept, in their own section - nothing re-anchors them                            | each thread's anchor `content_hash` against the answer's  |
| The finish            | `finish review (n)` files the drafts as one round and draws `review left (R{n})`; a refusal draws the core's own words and keeps the drafts               | `submit_review`'s reply                                   |
| A refusal             | the server's sentence as its own line - missing, unreadable, over the ceiling, outside the session's tree, a text kind that is not UTF-8                  | the `error` naming `document_read`                        |
| Opening               | `Opening <path>...` while the read is out                                                                                                                 | the page's own state                                      |
| Empty                 | its own line, before any kind is drawn                                                                                                                    | an answer with zero bytes                                 |

## The rules

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
hand-off tool answers with the sentence and nothing opens, and an address
read back from the URL re-decides `reviewable` false, so the viewer never
asks - the sentence reaches a reader as the tool's answer and as a raw
read's error, never as a drawn line, because the kind is the path's
extension and a `.pdf` path is refused at the hand-off itself. A
PDF rides the platform's own viewer: WKWebView renders one natively (the
desktop app; measured), every browser does (Chromium measured), and the
Android WebView is the one surface with no PDF viewer of its own, where
pdf.js draws the pages from the same bytes the read answered with. Safari
is the same WebKit engine that was measured; Firefox ships its own pdf.js.

**The measure is 860px, centred.** A document is read, not scanned: prose,
text and the PDF viewer line up on one column, and a plain-text document
WRAPS inside it - the sheet has no sideways scroller and this page brings
none.

**A document that could not be opened says so where the document would
have been.** A missing file leaves the address alive: the page draws the
refusal, and the tabs' back still works, because a deep link to a document
that moved is a page the reader has to be able to leave.

**A comment anchors to the raw file's numbering, and the document's own
hash places it.** A reviewable document therefore draws as lines, not as
rendered blocks: a comment's anchor is the line it was written on, with its
neighbours captured. The anchor carries the WHOLE document's hash - the
same FNV-1a a code thread hashes a line with, over the document's bytes -
so a comment written against this exact content sits at its line, and one
written against another version draws outdated at once, with its captured
lines kept. Nothing re-anchors a document automatically: a code review
re-finds a moved line by its content, and a document has no diff to move
within.

**The drafts outlive the viewer.** Unsent comments are held per document,
module-wide, so a second hand-off that raises another document does not take
the first's unsent comments with it - they wait in their own scope until
their finish files them.

**The finish is the review-left event.** `finish review (n)` submits the
drafted threads as one round through the same command a code review uses,
under the document's scope (`doc:<path>`), stamped with the asking seat as
the origin - so the agent hears it exactly as it hears a code review, the
review record holds it, and `review__list` names the document's scope among
the project's reviews.

## The states the page can be in

- **Markdown**: rendered with the chat's renderer - raw HTML never reaches
  the page - and the same `.prose` rules the conversation draws with.
- **A reviewable markdown or text document**: the line view instead - every
  line numbered and pressable - with its comments placed and its drafts
  under their lines.
- **A comment being written**: the composer opens under the line, with its
  own comment and cancel pair; a written comment draws as `drafted` until
  the finish files it.
- **A refused finish**: the core's own words beside the finish button, and
  the drafts stay exactly where they were - a refusal that cleared them
  would take the reviewer's words with it.
- **A review left**: `review left (R{n})` in place of the finish button.
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
- **A refusal**: each its own line, straight from the server's sentence.
- **A read that could not be sent**: the connection is down, in the page's
  own words - a silence that reads as still loading is worse.
- **A connection that dropped mid-read**: the reopen asks again rather than
  leaving the page opening for good.

## The pieces it takes from the pages beside it

- The `.wrap` column and the shared `.pending` line, from every page.
- `Prose` and `renderProse` are the chat's own: one markdown upstream
  (`markdown-it`, raw HTML off), one set of `.prose` rules, so a document
  and a quote from one render alike.
- The comment path is the review page's own: `upsertCommand` and
  `submitCommand` from `review/write`, and `authorName` from `review/view` -
  one writer, one vocabulary, so a doc comment and a code comment are one
  kind of thing on the wire.
- The page's own classes are `doc-*` under a `.doc` scope - a new component
  takes its own stem rather than reusing a bare name the sheet already
  styles.

## The widths

The page is checked at **1600** and at **430** against the drawing beside
it. The column is capped at 860 and centred, so the extra width at 1600 is
margin rather than line length; at 430 the column takes the page's padding
and nothing overflows horizontally. Long unbroken tokens wrap (`anywhere`),
and the PDF embeds at the column's own width.

## Touch

Every control the page carries takes the 44px target under
`@media (pointer: coarse)`: a numbered line target, the composer's comment
and cancel pair, and the finish button. Nothing lives on hover alone - the
number is drawn at rest and the composer opens on a press - and the plain
and PDF states carry no controls at all (the platform PDF viewer's chrome
is the viewer's own).
