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

| Region        | Shows                                                                                                                                                                                                                                                                           | Read from                                                          |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| Header        | the back link to the seat, `Review`, the branch, the round count, and the loop's state - `N replies to read`, in the warn tone while any thread waits on the reader, and `N open`                                                                                               | the record's `work.branch`, and the two review halves              |
| Tabs          | `Conversation` and `Files changed`, with the commit count                                                                                                                                                                                                                       | the record's git chain                                             |
| Scope         | the scope on screen, a stepper, the keys, the unified/split picker, and the comments' stepper with its `N/M` count                                                                                                                                                              | the record's git chain (the commit list), and the scope's own threads |
| Tree          | the scope's files as their own hierarchy - a row per directory and per file, folders before files, indented by depth; a directory carries the files beneath it, and the file whose region the reader is inside is marked as the current one                                    | the scope's diff, the threads in that scope, and the canvas's own scroll |
| Canvas        | the scope's files as diffs, with each thread inserted under its own line, the composer under the line a comment is being written on, and GitHub's own gap strips between two hunks (above the first, below the last) that draw the lines the diff left out, a press at a time      | the scope's diff, the threads, the server's landing per thread, and the file's whole content read |
| Outdated      | every thread whose code could not be placed, drawn against the lines captured when the comment was written                                                                                                                                                                      | the threads half                                                   |
| Rounds        | newest first: the round's number, its age, its tally by state, its summary, and each member thread                                                                                                                                                                              | the rounds half                                                    |
| Finish        | the summary field, its mic, and `finish review`, live once a comment is drafted - the conversation half's own section, and, on the diff half, a `finish review` control under the canvas that opens the same row in place and collapses again once the round seals                | the page's own drafts                                              |
| The chat line | the lines the view this client replaced sent, pushed into the reviewed seat's own chat: `Review #N ready - address it via the review MCP (review__list)` when a round seals, `Reopened a comment in review #N - take another look via the review MCP (review__get)` on a reopen | the page's own dispatch after the write                            |

## A scope, and where a thread draws

**A scope is All changes or one commit.** A thread draws in the scope it was
written in, and in all changes whenever the content that scope records still
carries its line:

- A commit-scoped thread sits at its recorded line in its own commit's view.
  A commit's diff is immutable, so that line is settled by construction -
  there is nothing to re-resolve. In the all-changes view it draws where the
  server's landing puts it: the record resolves a commit-scoped anchor
  against the branch's content too, by content and neighbours alone (its
  recorded number counts in the commit's numbering, so it is never the
  evidence), and one the branch no longer carries parks as outdated there -
  its own commit's view still draws it where it was written.
- An all-changes thread sits where the server's landing puts it. The record
  carries, per thread, the resolver's own answer against the content it
  ships - placed at a line (with the line it moved from, when it moved), or
  outdated with the resolver's reason. A thread with no landing carried -
  a read that answered none - is parked rather than guessed onto its
  recorded line.
- **A commit's view draws only its own threads.** An all-changes thread's
  numbering is the branch's and the record carries no landing for it against
  a commit's diff, so placing it there would be a guess the resolver refuses
  to make.

The keys, as the keycap row under the tabs states them: `←` and `→` step one
scope and `shift`+`←`/`→` step one comment, both **cycling** - the last
steps round to the first, so a held arrow walks the branch rather than
hitting a wall - `a` flips between all changes and the commit you were on,
and `j` opens the picker (arrows and Enter; Escape closes). The brackets do
the same stepping as the arrows and are not advertised beside them. The
comments are walked in the order the canvas draws them, by path and then by
line, and each step brings its card into view. Both walks have buttons in
the tabs row - the scope's stepper and, beside it, the comments' stepper
with its `N/M` count - because a key alone is no door on a touch screen. The
keys are live only while the diff half is on screen, and a keypress in a
text field is the field's.

The commit list is the record's own; a commit's diff is fetched when its
scope is first shown, and not carried on the record - a branch can hold
dozens of commits, and every reader of a record
would pay their content on every read for a scope only this page draws. An
ask the socket did not take is asked again, so a scope opened while the
connection was down recovers rather than reading for ever.

**The tree draws the scope as its own structure**, not a flat list of paths:
one row per directory and per file, folders before files at every level,
indented by depth, and a directory's row carries every file beneath it -
which is what its collapsed row hides. A file states a side of its change
only where it has one, so a file with no removals carries no `-0`, and
each file row carries its full path as its hover title.

**The tree also says where the reader is.** The file whose region holds the
canvas's reading edge - its own top at the wide width, the window's where
the page is the scroller - is marked as current, and the mark moves as the
scroll moves, one mark a frame. A reader three files into a long branch does
not have to hunt for their place in the list.

**The canvas draws what is on screen.** The sections are virtualised over
files - the browser holds the two or three a reader can see and the rest
stays data - which is what makes the unified/split switch fluid: the mode
change re-renders the tables on screen, not every table on the branch
(measured at about ninety milliseconds after the change, against
three-quarters of a second for the whole branch). The tree's rows jump to a
file the list has not drawn yet.

**A diff's hunks leave lines out, and the page can draw them.** The record
carries bounded hunks, so the lines between them - and above the first,
below the last - are exactly what it does not carry. The controls are
GitHub's own, down to the glyph and the placement: a strip in the hunk
header's line-number column, on the row the diff already draws, with the
gap below the last hunk getting a row of its own under the file's last line;
hovering lights the strip. A gap with no more than twenty lines left opens
in one press and takes the two-way unfold mark with GitHub's `Expand All`;
a longer one takes a strip per end it can be opened from - the fold-down
mark above the fold-up one between two hunks, and a single direction at the
file's edges - each press drawing twenty lines from the end it names. On a
press the hidden lines draw as context around the hunk they sit with: the
diff text is re-emitted with the heading grown by them, because the viewer
numbers every row from that heading and rows spliced in by hand would draw a
numbering it never agreed to, and a gap opened all the way joins the two
hunks into one block with the second heading gone, the way GitHub closes one.
The lines come from the file's whole content read at the scope's own
revision - a commit's tree for a commit's scope, the branch tip for all
changes - bounded by the same ceiling a document read is, and answered as a
flagged short read at that ceiling rather than as a refusal, its lines
capped at ten thousand besides. **The page asks for that read when it draws
a file that can have a gap** - not a deleted file, a binary, a submodule, an
added one whose single hunk is its whole content, or one whose hunks the
record itself cut, none of which a control can ever fill - because the
answer decides which controls exist: a read that could not run, or one cut
before the gap it would fill, earns none rather than a door that opens on
nothing.

An expanded line takes no comment door and no click to comment. The record
does not carry it - the file read does - and a thread's anchor is resolved
against the content the record ships, so a comment written there would park
as outdated the moment it was filed. The door is offered only on the lines
the hunks carry.

## A thread

The state chip, then the anchored path and line, then the turns: the
reviewer's read as `you` behind an accent rule, an agent's by its label
behind a plain one. A turn carries one meta chip beside its name: the round
that sealed it, which only a reviewer's turn has, or else its age from the
second turn on, because the thread's opening turn has nothing to be later
than. Both halves of the page draw the same turn through the same piece, so
a chip is a chip wherever the turn is read.

A turn's words render as markdown, through the chat's own renderer: a reply
carrying backticks, `#[async_trait]` or a fenced block reads here the way it
reads in the conversation, and lines typed into a comment stay lines.

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
on that line's side.

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

The page writes through the core's review commands: a comment is an
`upsert_review_thread` carrying the thread, a
reply the same command with the reviewer's turn appended, a resolve or
reopen a `set_review_thread_status`, and `finish review` a `submit_review`
sealing every thread this sitting has drafted or replied on. The summary
field is the round's own.

**A comment opens with the caret already in it.** Clicking a line's door -
or anywhere on the line - is a reader asking to write there, so the box's
field takes the focus as the row mounts; a click on the line the box already
sits on puts the caret back in it.

**The diff half can finish the round where the reader is.** The row under
the canvas carries a `finish review (N)` control while the round is open;
a press opens the page's own finish row in place - the same summary field,
the same seal, one snippet drawn by both halves - the field takes the
caret, and Enter (or the button) seals the round and collapses the row
again. A reader never has to leave the diff to close a round, and the write
path is the conversation half's own either way.

**The finish nudges the seat the review belongs to.** A round that sealed
also sends a `prompt_under` into the reviewed seat's own chat - one line,
never the comments, which are what the review MCP is for: `Review #N ready -
address it via the review MCP (review__list)`, the round's own number in it,
the words the view this client replaced sent on its own seal. The worker
therefore hears a review is ready and reads it back, live and on a fresh
read. **Only a round with work in it is worth the nudge**: a round whose
sealed threads are all resolved or outdated files without one, the predicate
the view this client replaced sealed by, and a reply that carried no round
nudges nobody - a worker sent to read a review that does not exist is worse
off than one told nothing. A nudge is best-effort besides: one that could
not leave is the worker's own line missing, never a lost review, so its
failure is drawn nowhere.

**A reopen nudges the same seat the same way.** Flipping a thread back to
open hands the work back to the worker, so the line names the round the
reopen is about - the thread's latest filed one, never its origin:
`Reopened a comment in review #N - take another look via the review MCP
(review__get)`, or the round-less `Reopened a review comment - take another
look via the review MCP (review__list)` for a thread no round has filed. A
resolve does not nudge at all: a thread read and accepted needs no second
look.

**The anchor is captured where the lines are.** The comment is written on a
line of the drawn diff, so the page takes that line's text, its neighbours
within the hunk, and the crate's own hash of the line - the capture the
crate's resolver defines - and the thread carries it. The
hash is FNV-1a over whitespace-collapsed UTF-8, and the vectors the client's
implementation is pinned against live in the crate's resolver tests too: a
hash the two disagree on would leave a thread written here unplaceable when
the core resolves it.

The hash crosses as decimal digits rather than a JSON number. JS numbers are
doubles, exact only to 2^53, and the hash is a full u64 - so the anchor's
serialization is the digits, and the deserializer accepts a number too,
which is what blobs written before this held. A thread read back is echoed
verbatim on a reply, so a round-trip cannot round the hash off.

Every write re-reads the record when it lands, because nothing pushes a
review change: the drawn round, the drawn thread and the header are always
the store's own answer rather than the page's guess at what the write did.

**Every box on the page dictates, the way the composer's box does.** The
comment box, the reply box in either half and the round's summary field each
wear the composer's own mic control - a press starts a take, a hold opens
what dictation is set to - over the page's own take, so a take begun here is
addressed to the seat this review belongs to and the server sees an ordinary
take on that seat. The summary's own row is drawn by whichever half is up,
and it goes with that half: a take begun in the diff half's finish row ends
when the row does.

**The visible take bar takes the line of the box that began the take**,
exactly as the composer's bar takes the draft's own line: the row the words
would be typed into is the row they arrive on, so nothing is added and
nothing moves as a take starts or ends, and no other box draws a bar for a
take that is not its own. The words land at the end of the box that began
the take, if it is still open when the take settles - a box the reader
closed has nowhere to put them, and the words are dropped rather than
written where the reader is not. A notice about a take this page never
watched (a page that attached or reloaded on the previous take's words) is
not placed at all, and a take that could not start says so in the box it was
pressed in - the line belongs to the attempt, not to the next line the
reader opens.

**The push-to-talk key is one registration for the page**, because the
microphone is the client's own and every box drives the same take: a
registration per box would fire the key once per open box. It begins its
take in the box the reader is in, else the one open, else the summary - the
same press and release the composer's box reads - and it is offered only
where a take has somewhere to land, since a take whose words no box can take
is a take the reader loses. Every box on the page is covered: the comment box
on a diff line, the reply box under a thread, and the round's summary,
wherever it is drawn.

**The write's outcome is read.** The two commands that answer through a
reply carry the server's own answer back - the store's yes, or the word a
refusal came as - and a dead socket throws before anything goes; either way
the draft is added only once the write landed, the box that wrote it keeps
its words, and the refusal is drawn where the reader acted -
at the top of the page at the wide widths, and riding the top of the
viewport at the narrow one, where the single column can carry the reader
far past it. `finish review` keeps the summary and the drafts on a refusal
rather than clearing words the round never filed, and reads the minted
round's number out of the reply's own body - that number is what the nudge
carries.

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
- **A file's content being read**: no between-hunk control is drawn until
  the read lands, and a read that failed, refused or was cut before a gap
  draws none for it - the controls are only ever doors onto lines the read
  actually carries.
- **A take running**: the take's bar in the line of the box that began it -
  the composer's own reading, `ready`, `recording` and `transcribing` - and
  the mic beside it says `stop dictating`; the words land in that box when
  the take settles.

## The strip's row

The seat's own page carries the door: the strip above its composer draws a
review row wherever the tree row draws - a branch of its own, a dirty tree,
a pull request, a tree that could not be read - labelled `N to read` while
any thread waits on the reviewer, the thread count otherwise, and `review`
when nothing is filed. Its hover panel lists `open review`, then each
thread by its file and line with its state and the turn a reader would be
reading; a branch with nothing filed draws the row with no thread lines,
and a threads read that did not land draws as itself, one row naming the
failure. Neither row draws on a clean default branch, which has no tree
state to state.

## The two widths

Checked at **1600** and at **430**. The header wraps rather than scrolling
sideways; the tree and the canvas stack below 980px and the page scrolls as
one column.

## Where the surface differs from `web-session.html`

The session page's drawing named a diff review overlay as its sixth section,
deliberately not built at the time. The surface landed as a route of its own
instead, so a review is addressable and a second reader can sit on it while
the session page draws the conversation.
