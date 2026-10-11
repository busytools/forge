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

## What it draws

| Region | Shows | Read from |
|---|---|---|
| The document | markdown rendered by the chat's own `Prose`; plain text as wrapping text; a PDF in the surface's own viewer; any other kind named as itself | the `document_read` answer: `document_kind` and the bytes |
| A refusal | the server's sentence as its own line - missing, unreadable, over the ceiling, outside the session's tree, a PDF handed reviewable, a text kind that is not UTF-8 | the `error` naming `document_read` |
| Opening | `Opening <path>...` while the read is out | the page's own state |
| Empty | its own line, before any kind is drawn | an answer with zero bytes |

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
reason.** The refusal is the read's, so every door onto a document - the
hand-off tool, the viewer's own route - refuses with the same sentence. A
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

## The states the page can be in

- **Markdown**: rendered with the chat's renderer - raw HTML never reaches
  the page - and the same `.prose` rules the conversation draws with.
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

The page carries no controls of its own - the platform PDF viewer's chrome
is the viewer's, and everything else is text. The reading column is the
whole of the page, so there is nothing under 44px to reach.
