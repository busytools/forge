# A session

The seat's own page: the conversation in the middle, the projects rail
beside it, and at the foot the surface the [queue pile](./queue.md) and
the [composer](./composer.md) share. It is addressed by the seat's slot -
`/session/{org}/{project}/{label}` - so a seat is a URL a deep link can
reach; `/new` and `/resume` swap the occupant and leave the slot alone.

It is drawn from one read of the `session` subject - a snapshot, then the
updates that move it - plus the home's snapshot, which the page is handed
for the facts that ride a project's row. The drawing it is held against is
[web-session.html](./web-session.html), beside this page.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Rail | every declared project grouped by the strongest state among its rows, the current seat marked, sleepers folded; its footer carries the account and what it is costing | the home's snapshot; `accounts` |
| Header | the chip, the seat's own mark, org and name, and the facts: session id, model with its effort, mode, context fill, and the compaction count | `header`, `conversation.compaction_count` |
| Conversation | the turns and everything inside them, in the column's own scroll viewport | `conversation` |
| Queue | the prompts waiting, drawn against what is running | `queue`, `queue_ended` |
| Strip | the segments this seat has anything to say in, in one order, standing whenever it has something to say | the seat's record, the project's home row, the page's clock |
| Foot | the strip lane, the dock or the box | the composer's own record |

**The chip is the one projects control, and its click is smart.** It
counts seats that want a person - a question, a permission, a failed turn,
never a mere change - and exactly one, not an error, goes straight to that
seat; several, or any failure among them, opens the home to pick from;
none goes home too, from the quiet `projects` word. Hovering it summons
the rail as a peek. It is a real anchor, so keyboard, middle-click and
new-tab all behave. **A worker's row in the rail names the tree it runs
in** when a lead pointed it at one of its own: the path alone, a little
under a failure's sub-row, hung under the worker's own name so that name
stays whole on its line - and on hover it slides left to show whatever
the rail was too narrow for. Its branch and files reading a tree other
than the project's is then expected rather than a fault.

**The header reads the seat, not the app.** The name leads and the org
qualifies it only where the fleet makes the name ambiguous (#1707); the
session id is shown in its slot; effort rides the model it belongs to,
because it is a property of that choice; the context figure is one unit -
how full the conversation is, and how many times it has been cut - and the
compaction count is a control that takes the reader to the latest cut. A
usage nothing has reported draws no track and a dash, never a bar at zero.
Everything the row must fold stays one tap away under `more`.

## The run of rows

Every call, message and thought draws on its own row, in the order it
arrived, and the list only appends: no kind gets a row of its own, the
kind's glyph rides the row that draws it, and nothing rearranges under the
reader. A message is a tool row and so is a thought - the same rows and
marks a run of calls draws - which is what makes traffic and reasoning
read as one system.

- **A family row** carries a tool family's glyph: read, search, bash,
  web, lsp, skill, tool search, config, worktree, tool, and the one
  mutation row every edit draws whatever tool it was.
- **A forge call** draws a card for its family - tasks, agents, cron,
  documents, gotify, review, slack - and the System One decisions draw as
  their own family, so a reader scanning for what the session decided does
  not find the three mixed among its cron and peer calls.
- **A browser call** is the browser's, whatever server name carried it.
- **A peer message** draws a card saying which way it went: `sent to` and
  `from` name the counterparty and its org, `failed` carries the
  delivery's own reason, and `whoami` and `list` open onto their facts.
- **A delivery** - a cron fire, a Slack message, a Gotify push - is a row
  of its own kind, a cron fire titled by the schedule that fired it. A
  Slack row whose block is late adds its age to the title, so a week-old
  message reads as history before the row is opened.
- **A nudge** - a chase warning, a browser's idle message, a worker's
  kick - is the machinery's own words, so it draws as a work row under
  the board's own mark rather than as the reader's: nobody typed it, and
  it never wears `.mine`. A long one - a user-wait nudge, a browser
  sentence - opens on the whole of what it said, the way a thought row
  does. The row is found by the id the machinery minted for the prompt,
  which survives into the transcript, so a reloaded row draws what the
  live one did.
- **An edit** draws its diff: the hunks with both line numbers, and the
  marks that say how each line moved.
  **An edit or a question whose structured record the page could not
  carry** says so on its marks line: an `omitted` figure with its size,
  scaled to the unit the size belongs to (`omitted, 5.0 MB`). On an edit it
  is one more mark beside the hunk count; on a question, which has no hunk
  count, it is alone on that line. A record over the cap
  first keeps the keys the row draws and drops only what nothing reads, so
  a large edit keeps its patch and a Write draws the file it created from
  the call's own input, and a question whose huge typed notes pushed it
  over keeps its `answers`. **A question answered live and the same
  question read from the transcript draw identically** - the cap bounds
  both paths the same way - so the answer card is the card either way.
  Only a record with nothing drawable left falls back to the omission: an
  edit names it on the marks line, and a question draws as its own row,
  its question glyph and the question it asked, opening on the omission.
- **A backgrounded call** is held open by its task fact, its result
  saying the command started, and the ending landing later as a frame of
  its own. **Its output box follows a call that is still out**: while the
  row stays open and the call runs, the row re-reads the command's own
  output file on a one-second tick, and the tail it draws carries a `live`
  mark saying the box is still being written. A read that found nothing
  while the call runs says the output has not appeared yet: the file is
  there from launch and reads as empty until the command's first line
  lands, so the settled wording about a call that never recorded one would
  be a claim the row cannot make yet. Once the call settles the mark goes
  and the tail is the finished output.
  **The box draws the command's own colours**: every terminal box reads the
  escape sequences a command wrote - colour, bold, dim, italic, underline -
  into the theme's palette, so a coloured compile looks like the terminal's.
  A sequence the page cannot draw (a cursor move, an erase, the extended
  colour forms) is dropped rather than printed, and a command that wrote no
  colour draws in the box's own tone.
- **A message's image** draws as the file row a turn attached does: the
  picture named, and its source printed beside it. The page fetches nothing
  for it - a message is text someone else wrote, so the host it names is
  never asked for.
- **A settled failure with nothing to open onto** says so in words rather
  than drawing an empty body.

The conversation is cut by the server into turns, and the client folds
each turn's frames into its rows; a compaction boundary draws where it
happened, collapsed, with the header's count saying how many and the mark
saying where.

**A row is drawn before the list has measured it.** The virtualised list
places a row on an estimate-derived offset and corrects it the moment the
row reports its own height; drawn rather than left blank, so a fast scroll
through fresh history reads as moving content instead of a black column.
The correction is the list's, and the column's foot and parked anchor
absorb it, so the place the reader holds does not move under the reader.

**The column's top edge says when it is fetching.** An older page is
asked for as the reader nears the top and answered a moment later;
while it is in flight the wheel has nothing left to scroll, so a small
pill at the top edge says the older messages are loading rather than
leaving the column reading as a dead scroll. The reader's own page is
sized by what the column has measured - about a screen and a half of its
own turns where the turns allow, one turn at minimum - while a walk
toward a distant row keeps the full page. **A conversation that fits the
room has no top to scroll to**, so it asks for the page above on the
fitting state itself and keeps asking as pages land, until the history
fills the room or ends.

## The strip

One lane of segments above the foot, in a fixed order: the browser, the
agents, the processes, the connectors, the schedules, the MCP servers, the
files, the tree, the review, the tasks, the monitors. Each segment draws
what it has and nothing else, and the strip stands whenever anything has
something to say - a running turn is one of the things it may say, not the
condition for it, because these rows are the home of what the inspector
used to hold.

**The files slip is the one mandatory segment** (Ved, 2026-10-17): every
seat has a tree - a worker's own worktree, the lead's checkout - so it
draws for every seat with a record, where the tree row beside it keeps its
rule of staying away when nothing has moved. **The slip IS the door, with
no panel**: a press opens the page that reads the whole tree
(`/session/<org>/<project>/<label>/files`), reading `walk · files` for a
seat in a worktree of its own (the worktree's name, so the destination is
never a guess) and plain `files` for the project's checkout, with the
browser's own mark beside it and no count. No hover dialog, no panel -
there is nothing here but the way in.

**Every cell is a read, not a computation.** The schedules row keeps the
seat's own set - a cron names the seat that created it - and its
countdowns re-derive against the page's own clock, as the monitors' ages
do; the tasks row reads the project's slice of the home; the tree row is
the seat's own working tree, the worktree for a worker and the project's
directory for a lead, and a worker a lead pointed at a tree of its own is
named by that tree - `~/Projects/nazar` beside its project's siblings -
rather than described. A tree that could not be read says so rather than
drawing a blank. The account a row chips is on the rail's footer
beside what it is costing, not on the strip.

**The review row's panel is the review sets.** The filed rounds draw
oldest first with the latest at the bottom - the conversation's own
direction - each a summary line and a tally of its threads' states; under
it a thread collapses to its file, its line and its toned state, and a
click opens the commented code (the branch diff around the line the
server resolved the anchor to, or the lines captured with the comment
where it has moved on) beside the whole thread, its turns in order.

## The states the page can be in

- **A seat with a conversation**, which is the ordinary case.
- **Waking**: a seat nothing has started. Opening a lead's page starts it,
  and a worker's seat with nothing behind it draws `not running` and the
  reason, because a worker is spawned by the lead that owns it. A seat
  this client just closed is never asked: the roster landing it asleep is
  that close arriving, not a wake to start.
- **A seat whose session died**: the row carries the death with the reason
  it left, drawn with the failure mark, until the seat is shown - even
  after the seat is back up, when its lifecycle reads what it is now. The
  trace is spent by any subscribe to the seat, whichever page made it, and
  an opened lead's page starts a died seat again the way a sleeping one is.
  **What stays parked**: a Failed row with no death behind it, which is a
  spawn that never came up - the reason line says which - and a seat
  waiting on sign-in, whose lifecycle is its own and is never woken. The
  recovery that brings a died seat back says so in the chat - the session
  resumed, or a new one under a new id.
- **A refused read**: the server turned the subscription down, and its own
  words are drawn.
- **A connection that dropped**: the page keeps what it last read, and the
  rail's footer says so where the build facts live.
- **A forge on another protocol**: the same footer line names the skew; the
  page behind it is live, and nothing is refused over a version.

## The two widths

The page is checked at **1600** and at **430**, against the drawing beside
it. **The rail is summoned at every width** - it is never a column, so
nothing of the conversation is spent on it while it is away: a hover over
the chip is a peek, Cmd+Left or the palette opens it to stay, and Escape
or Back closes what it opened. At 430 the chat keeps its single column,
the facts row folds under `more`, and nothing overflows horizontally at
either width.

**Touch is the rule, not a fallback**, and this page is where it is held:
the client runs on Android, so every control a finger reaches takes a 44px
target under a coarse pointer, and the rail's close chip is the one
affordance hover uncovers - a touch screen keeps it shown and a keyboard
focus reveals it, so it is reachable either way.

**The system bars are the page's own edges.** The Android window draws
edge-to-edge, so the shell measures the status bar and the gesture bar and
writes the four lengths as `--safe-*`; the header row, the chat's foot and
the summoned rail are each padded by them. A tap in the status bar's band
goes to the system before the page sees it, so the chip and the palette's
magnifier - the touch door to the rail - have to sit clear of it.
