# The files

One seat's whole working tree, readable and commentable: the tree, a
read-only reader over each file, and the comments dropped on what was read,
listed beside the reader. It is reached by its URL, by the back link in its
header, and by the strip's files slip on the seat's own page - one place,
and the tree follows the seat. The drawing it is held against is
[web-files.html](./web-files.html), beside this page.

**Two halves, like the review page's**: the **Files** tab is the tree, the
reader and the comments rail - which carries the dropped list and the
finish and is that tab's alone - and the **Conversation** tab is the
seat-tree scope's rounds, newest first, taking the whole row: the same
conversation the review page draws of a branch, through one component both
pages use.

It lives at `/session/{org}/{project}/{label}/files`. **The tree is the
seat's own**: a worker's worktree when the seat is a worker, the lead's
checkout on the lead, which is the directory the record already reports as
the seat's scanning cwd. Nothing here reads the launch directory, and the
tree needs no new walk: the seat's file index - what the composer's `@`
list already reads - is the tree.

## Where a comment is filed

A comment dropped here is filed under a **seat-tree scope**, spelled
`tree:{org}/{project}/{label}` - a third scope beside the branch scopes and
the document scopes the rest of the review plumbing uses. The scope is the
seat and never the branch, so a worker that commits, or rebases, or moves
its branch keeps its comments; a comment threads, replies, resolves and
seals into rounds exactly as one on a diff does, and the worker addresses it
through the same review MCP. When the reader seals a round,
`finish review` files a `submit_review` and pushes `Review #N ready -
address it via the review MCP (review__list)` into the seat's chat, the
review page's own line.

## What it draws

| Region        | Shows                                                                                                                                                                                                                       | Read from                                                                              |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Header        | the back link to the seat, `Files`, the seat's resolved tree and its file count, `N to read` while any thread waits on the reader, the `Conversation` / `Files` tabs, and the narrow doors                                    | the record's `state.scan_cwd`, its `file_index`, and the seat-tree threads             |
| Conversation  | the rounds newest first, each with its age, tally chips, summary and threads - state chip, `path:line`, the turns with `edit`, and `respond` / `resolve` / `reopen` / `delete` (an accepted thread collapses to its marker, opened by a press, where only reopen, collapse and delete sit); parked threads above in an Outdated group with their captured lines | the record's `tree_reviews`; the page's own drafts and reply box                       |
| Tree          | the seat's whole index as its own hierarchy - a row per directory and per file, folders before files at every level, indented by depth - with the current file marked, a pill per file carrying its comment count, and a filter | the record's `file_index`; the filter ranks by the `@` list's own matching             |
| Reader        | the file's head - path, language, line count - and the file's text, read-only, colour and structure as the language set allows, wrapped at the box's edge; the target line carries the tint and the gutter `+`                | the `document_read` answer for the open path: kind and bytes                           |
| A thread      | under the line it was written on: its state chip, where it is, its turns through the review page's own turn renderer, `edit` on a turn the reader wrote, and `respond` / `resolve` / `reopen` / `delete`; a resolved one collapses to its marker line, opened by a press | the record's `tree_reviews` threads and their landings, and the page's own drafts      |
| A comment box | under the line a press landed on, the caret already in it; **Enter comments**, Shift+Enter keeps the newline                                                                                                                | the page's own comment shelf                                                           |
| Comments      | every thread in the scope as one list, by path and then by line: the state chip, `path:line`, the first words, and `delete`; a row jumps back to its line                                                                    | the record's `tree_reviews` threads                                                    |
| Parked        | a thread whose landing could not place it - the code moved too far, or no landing was carried - says so on its row, its captured lines kept; it is never dropped                                                             | the landing's `outdated` word, or none carried                                         |
| Finish        | the round's summary - a roomy, resizable box, present by default in the rail's bottom, its own mic like every box - over `finish review (N)`, which seals the sitting into one round; a refusal keeps the words                | `submit_review`'s reply, and the round's number                                        |
| States        | reading; a seat the server refused, in its own words; a tree with no files; no match for the filter; a kind this page draws no text for; a file read refused with its own sentence; **a seat-tree review read that failed or answered a shape this build cannot name, drawn in its own line rather than folded to the clean empty page** | the record's and the read's own outcomes                                               |

## Reading

The read is the document read - **the server carries the file's bytes, the
client draws them** - so it resolves against the seat's tree, refuses with
its own sentence (missing, unreadable, over the ceiling, outside the tree,
not UTF-8) and answers the file's kind. The reader shows the bytes from
when the file was opened and re-reads on a re-select: an open file is a
snapshot, never rewritten under the reader's eyes while a worker edits it.

The pane is a read-only CodeMirror instance, coloured by shiki's github-dark
family - the same the chat's code blocks draw - over the JS regex engine,
with no wasm. Each language has two layers, neither guessed: structure and
folding from the CodeMirror org's own package where one exists, and the
colouring from shiki's grammar either way; `.svelte` and `.toml` are
colour-only, and a path with no known language draws as plain text. The
grammars are all loaded with the reader's own chunk, so a language's first
file pays nothing extra, and the whole chunk - CodeMirror, shiki, the
grammars - is pulled only when the reader first draws a file. A file that
is not text (an image's bytes are not a rendering to invent) draws a line
saying so.

**A press on any line is a comment**: the line takes the tint and the
gutter `+`, and the box opens under it with the caret in. Doors are press
targets at every pointer, and the reader's own scroll is capped by the
sheet's rule that nothing scrolls sideways.

## The thread, and the round

An anchor captures what the review page's anchors capture - the file's
path, the line, the line's whitespace-collapsed hash, and its neighbours as
context - from the same lines the reader drew. The landing is resolved by
the **server**, because the client has no working tree to re-scan: the
record carries, per thread, either the line it sits on now (with the line
it moved from, when it moved) or the resolver's own reason it could not be
placed. A thread whose file has vanished, or whose seat has no resolvable
cwd, is carried with no landing and parks.

`respond` appends the reviewer's turn and flips the thread toward the
worker; `resolve` marks it read; `reopen` returns it and nudges the seat.
Every write re-reads the record, because a review change is not pushed -
the reviews ride the record, and a record re-read is what carries them.
The Conversation tab answers over the same calls a diff review does - reply,
resolve, reopen - and a rail row jumps back to its line from either tab.

**A comment is editable and retractable wherever its card is drawn** (Ved,
2026-10-17): `edit` on a turn the reader wrote opens the page's own box over
that turn's words - the same mic and take as every box - and `save` upserts
the text with the turn's identity kept; `delete` removes the whole thread,
and it asks before it writes, because a delete cannot be undone. Both write
through the review plumbing every other change rides.

## The two widths

At 1600 the page is three columns: the tree, the reader, the comments. At
430 the reader is the page and the other two are sheets behind two buttons
in the header - `tree` and `comments (N)` - each pushing a history entry so
the hardware Back closes what it opened, with `done` and Escape as the
other doors.

## The slip

The seat's own page carries a **files slip** in the strip above the
composer, before the tree row: a bare door, the files mark and the label
naming the tree it opens - `walk · files` for a seat in a worktree of its
own, plain `files` for the project's checkout. A press opens the page, and
there is no count and no panel: the slip IS the door. It is the one
**mandatory** segment - every seat has a tree - where the tree row beside
it keeps its rule of staying away when nothing has moved.
