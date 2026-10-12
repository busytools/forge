# Review

One seat's reviews, addressed at
`/session/{org}/{project}/{label}/review`: the loop's state, the threads
whose code has moved out from under them, the diff they are anchored to, and
the writing - a comment on a line, a reply, a resolve, a round sealed. The
drawing it is held against is [web-review.html](./web-review.html), beside
this page.

The route is a sub-path of the seat because the seat is what a review is
keyed to: the record's review read is the seat's own branch, so reviewing a
worker's work means opening that worker's seat. The page is reached by its
URL, by the back link in its header, and by the strip's review row on the
seat's own page.

## The two halves

**Conversation** draws the review as it ran: the parked threads, then the
rounds newest first.

**Files changed** draws the review as it is anchored: the scope control, the
file tree, and the diff with the threads on their lines.

| Region   | Shows                                                                                                                                                                             | Read from                                                          |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| Header   | the back link to the seat, `Review`, the branch, the round count, and the loop's state - `N replies to read`, in the warn tone while any thread waits on the reader, and `N open` | the record's `work.branch`, and the two review halves              |
| Tabs     | `Conversation` and `Files changed`, with the commit count                                                                                                                         | the record's git chain                                             |
| Scope    | the scope on screen, a stepper, the keys, and the unified/split picker                                                                                                            | the record's git chain (the commit list)                           |
| Tree     | the scope's files as their own hierarchy - a row per directory and per file, folders before files, indented by depth; a directory carries the files beneath it                    | the scope's diff, and the threads in that scope                    |
| Canvas   | the scope's files as diffs, with each thread inserted under its own line and the composer under the line a comment is being written on                                            | the scope's diff, the threads, and the server's landing per thread |
| Outdated | every thread whose code could not be placed, drawn against the lines captured when the comment was written                                                                        | the threads half                                                   |
| Rounds   | newest first: the round's number, its age, its tally by state, its summary, and each member thread                                                                                | the rounds half                                                    |
| Finish   | the summary field and `finish review`, live once a comment is drafted                                                                                                             | the page's own drafts                                              |

## A scope, and where a thread draws

**A scope is All changes or one commit.** A thread is drawn only in the
scope it was written in, because the numbering its anchor records is that
scope's:

- A commit-scoped thread sits at its recorded line. A commit's diff is
  immutable, so that line is settled by construction - there is nothing to
  re-resolve.
- An all-changes thread sits where the server's landing puts it. The record
  carries, per thread, the resolver's own answer against the content it
  ships - placed at a line (with the line it moved from, when it moved), or
  outdated with the resolver's reason. A thread with no landing carried -
  a read that answered none - is parked rather than guessed onto its
  recorded line.

The keys, as the keycap row under the tabs states them: `←` and `→` step one
scope, `a` flips between all changes and the commit you were on, and `j`
opens the picker (arrows and Enter; Escape closes). The brackets do the same
stepping and are not advertised beside the arrows. They are live only while
the diff half is on screen, and a keypress in a text field is the field's.

The commit list is the record's own; a commit's diff is fetched when its
scope is first shown, and not carried on the record - a branch can hold
dozens of commits, and every reader of a record, the terminal included,
would pay their content on every read for a scope only this page draws. An
ask the socket did not take is asked again, so a scope opened while the
connection was down recovers rather than reading for ever.

**The tree draws the scope as its own structure**, not a flat list of paths:
one row per directory and per file, folders before files at every level,
indented by depth, and a directory's row carries every file beneath it -
which is what its collapsed row hides. A file states a side of its change
only where it has one, so a file with no removals carries no `-0`, and each
row carries its full path as its hover title.

**The canvas draws what is on screen.** The sections are virtualised over
files - the browser holds the two or three a reader can see and the rest
stays data - which is what makes the unified/split switch fluid: the mode
change re-renders the tables on screen, not every table on the branch
(measured at about ninety milliseconds after the change, against
three-quarters of a second for the whole branch). The tree's rows jump to a
file the list has not drawn yet.

## A thread

The state chip, then the anchored path and line, then the turns: the
reviewer's read as `you` behind an accent rule, an agent's by its label
behind a plain one, and the reviewer's own turns carrying the round that
sealed them. An agent's turn carries no round, because only reviewer turns
file into rounds.

The four states are the server's own: `open`, answered by nobody yet;
`addressed`, the agent has answered and the ball is with the reader;
`resolved`, read and accepted; `outdated`, the anchored code moved too far
to place. A resolved thread on the canvas collapses to its marker line; in
the conversation it draws in full like every other thread. The set of
threads the Outdated section lists is the page's own reading of the record:
a thread whose landing says outdated parks, and so does one whose read
carried no landing at all - whether the store's status agrees is not the
test, because a store status was written by whichever view last resolved and
cannot say where the code stands now. A thread with no rounds filed yet
draws nothing in the conversation; it is a draft until a round seals it.

**A thread is drawn where its own side is.** An old-side thread - a
comment on a removed line - draws under the removed line's row, and in
split view the card holds the half it was written on rather than spanning
the lane; in unified it takes the whole width, as the line itself does. The
same rule runs the doors and the drawing: one door per side's number cell
in split, one per row in unified (an added or context line is the new side,
a removed line the old), and a click anywhere on a line opens the composer
on that line's side - the terminal's own reading of the same gesture.

The loop's `N replies to read` is derived rather than served: it counts the
threads in `addressed` or `outdated` whose last turn is an agent's. An
`open` thread never counts, even with an agent turn last, because that state
is only reached by a reopen or by a thread's code returning - both the
reader handing the work back - and an author this build cannot name is drawn
without being counted, so the tally agrees with every other surface's.

A worker's answer reaches this page through the record: the reviews ride the
snapshot, so a re-read is what carries it, and nothing pushes a review
change. The terminal's activity line - the `worker addressed review #N` row
it drops into the chat - is that view's own and is not drawn here; the frame
also carries nothing this page could not read from the record itself.

## Writing

The page writes through the same command surface the terminal's overlay
writes with: a comment is an `upsert_review_thread` carrying the thread, a
reply the same command with the reviewer's turn appended, a resolve or
reopen a `set_review_thread_status`, and `finish review` a `submit_review`
sealing every thread this sitting has drafted or replied on. The summary
field is the round's own.

**The anchor is captured where the lines are.** The comment is written on a
line of the drawn diff, so the page takes that line's text, its neighbours
within the hunk, and the crate's own hash of the line - the same capture the
terminal's overlay takes from its own scan - and the thread carries it. The
hash is FNV-1a over whitespace-collapsed UTF-8, and the vectors the client's
implementation is pinned against live in the crate's resolver tests too: a
hash the two disagree on would leave a thread written here unplaceable in
the terminal.

The hash crosses as decimal digits rather than a JSON number. JS numbers are
doubles, exact only to 2^53, and the hash is a full u64 - so the anchor's
serialization is the digits, and the deserializer accepts a number too,
which is what blobs written before this held. A thread read back is echoed
verbatim on a reply, so a round-trip cannot round the hash off.

Every write re-reads the record when it lands, because nothing pushes a
review change: the drawn round, the drawn thread and the header are always
the store's own answer rather than the page's guess at what the write did.

**The write's outcome is read.** The two commands that answer through a
reply reject on a refusal, and a dead socket throws before anything goes;
either way the draft is added only once the write landed, the box that
wrote it keeps its words, and the refusal is drawn where the reader acted -
at the top of the page at the wide widths, and riding the top of the
viewport at the narrow one, where the single column can carry the reader
far past it. `finish review` keeps the summary and the drafts on a refusal
rather than clearing words the round never filed.

## The states the page can be in

- **A seat's reviews**, the ordinary case.
- **Nothing yet**: `Reading the seat...` before the first record arrives.
- **A refused seat**: the server's own sentence, drawn the way the sibling
  routes draw a refused subscription.
- **A half that failed**: the two reads fail apart on the server, so each is
  drawn apart here - a failed threads read leaves the rounds drawn, and the
  reverse - and a half whose outcome this build cannot name draws that word
  rather than being folded into a failure it is not.
- **Empty**: both halves read and nothing filed draws `Nothing has been
reviewed on this branch yet.`; threads drafted with no round filed says how
  many are drafted.
- **A scope with nothing in it**: All changes on a branch with no commits of
  its own says so; a commit that changed nothing says so; a scope whose read
  failed says that; and an unnamed layer state draws its own word.
- **A commit being fetched**: `Reading the commit...` until its answer
  lands.

## The strip's row

The seat's own page carries the door: the strip above its composer draws a
review row once the record holds a review read - `N to read` while any
thread waits on the reviewer, the thread count otherwise, and `review` on a
branch with none - and its hover panel lists `open review`, then each thread
by its file and line with its state and the turn a reader would be reading.
A read that has not landed draws no row at all, which is the same
distinction the page keeps between "nothing filed" and "not yet read".

## The two widths

Checked at **1600** and at **430**. The header wraps rather than scrolling
sideways; the tree and the canvas stack below 980px and the page scrolls as
one column.

## Where the surface differs from `web-session.html`

The session page's drawing named a diff review overlay as its sixth section,
deliberately not built at the time. The surface landed as a route of its own
instead, so a review is addressable and a second reader can sit on it while
the session page draws the conversation.
