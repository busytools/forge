# A session

The seat's own page: the conversation in the middle, the projects rail
beside it, and at the foot the surface the [queue pile](./queue.md) and
the [composer](./composer.md) share. It is addressed by the seat's slot -
`/session/{org}/{project}/{label}` - so a seat is a URL a deep link can
reach; `/new` and `/resume` swap the occupant and leave the slot alone.

It is drawn from one read of the `session` subject - a snapshot, then the
updates that move it - plus the home's snapshot for the facts that ride a
project's row and the board's snapshot for the row's task, its schedules and
its connector sets. The drawing it is held against is
[web-session.html](./web-session.html), beside this page.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Rail | every declared project grouped by the strongest state among its rows, the current seat marked, sleepers folded; its footer carries the account and what it is costing | the home's snapshot, the board's snapshot for each row's task, and `accounts` |
| Header | the chip, the seat's own mark, org and name, and the facts: session id, model with its effort, mode, context fill, and the compaction count | `header`, `conversation.compaction_count` |
| Conversation | the turns and everything inside them, and the handoff row where two sessions met, in the column's own scroll viewport | `conversation` |
| Queue | the prompts waiting, drawn against what is running | `queue`, `queue_ended` |
| Strip | the segments this seat has anything to say in, in one order, standing whenever it has something to say | the seat's record, the project's board row, the page's clock |
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
  documents, gotify, handoff, review, slack - and the System One decisions
  draw as their own family, so a reader scanning for what the session decided
  does not find the three decision calls mixed among its cron and peer calls.
  The handoff's exit call is its own family: its family is handoff, it wears
  the handoff mark rather than the workers one, and it carries the note it
  handed over.
- **A browser call** is the browser's, whatever server name carried it.
- **A peer message** draws a card saying which way it went: `sent to` and
  `from` name the counterparty and its org, `failed` carries the
  delivery's own reason, and `whoami` and `list` open onto their facts.
- **A delivery** - a cron fire, a Slack message, a Gotify push - is a row
  of its own kind, a cron fire titled by the schedule that fired it. A
  Slack row whose block is late adds its age to the title, so a week-old
  message reads as history before the row is opened.
- **A nudge** - a chase warning, a worker's kick - is the machinery's own
  words, so it draws as a work row under the board's own mark rather than as
  the reader's: nobody typed it, and it never wears `.mine`. A long one - a
  user-wait nudge - opens on the whole of what it said, the way a thought
  row does. The row is found by the id the machinery minted for the prompt,
  which survives into the transcript, so a reloaded row draws what the live
  one did.
- **A session start** is one row, not one per hook: the firing says what
  fired and how many hooks it carried, and opens onto them. A hook that
  finished without a word draws no row of its own inside it; one that
  failed or is still running stays, and a failed start counts its failed
  hooks in the closed row's words as well as taking the failure tone.
  Every hook still opens onto the whole of what it printed, a start whose
  hooks all finished silent opens onto a line saying so, and a hook
  outside a start keeps a row of its own.
- **A read** draws by what the file is, from the same per-format policy the
  Files reader uses: markdown renders as prose, code stays coloured with the
  file's own grammar (a `Justfile` is `just`, a script's shebang names its
  own), and a text file no grammar names draws plain with a caption saying so.
- **An edit** draws its diff the way GitHub does, inside the same panel a read
  draws - the panel's border and ground, with the format on the head when the
  file names one: the hunks with both line numbers, the file's syntax colours on every line,
  the row tinted for a removal or an addition, and the changed words of a
  paired removal and addition tinted darker. **A change the call's own input
  carries as two sides** draws the same way without positions: the lines the
  two sides share stay as context, and only the lines that differ are
  removals and additions - save a change too large to compare, which draws as
  one removed block and one added block.
  **A markdown edit renders
  instead**: the hunk draws as markdown, in that same panel, with removed
  words kept inline, struck and dimmed, and added words drawn normal; the
  source diff is not shown under it. **A markdown file the call created**
  renders as the document it is, with nothing marked: the change is the
  line-zero hunk a create writes, and the marks line already says the file is
  new. **A plain row** - a command's output, a
  text result - takes the same panel's border and ground, with no format strip;
  a hook run's output keeps the sheet's own inset rule.
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
  **A call the `TaskStop` tool stopped has no ending frame of its own**:
  the tool path sends no task frame and no registry change, so its own
  success answer settles the call - in whichever held turn the call sits,
  which is not always the turn the stop lands in - the row states `stopped`
  beside its title, and the processes row stops counting it. A stop whose
  call the page does not hold draws as a row of its own, named with the
  task it stopped.
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
- **A frame the page has no face for yet draws as itself, marked
  provisional.** An unknown top-level frame and an unknown content block, and
  the CLI's own system lines for a permission refusal, an API error, an
  informational line and a notification, each draw as a notice whose mark is
  the wire's own type. The open body lists what the frame carried as one-line
  labelled values - the tool and call a refusal names, an error's message,
  status and retry, an unknown frame's fields - and the frame itself sits
  behind a second control under them, so the payload stays reachable while
  the row's face is the facts. The frame is seen the day it arrives and can
  be given a real face later, rather than being dropped. A refusal also
  settles the call it names, which no result frame will ever settle, and an
  assistant question nobody answered keeps its call row marked `unanswered`
  once the turn has closed. Until then the dock is the question's row, so the
  mark waits for the close; after it, the answer card never drew and the dock
  has cleared, and the call row is the only trace left.
- **A model swap, a memory recall, a local command and a project's
  development intent each draw a row of their own.** A model fallback (and a
  refusal retry, a refusal with no fallback, and the Fable consent gate) draws
  a warning whose words are the CLI's own `content` and whose open body names
  the trigger, the model it left and the model it fell to. A memory recall
  draws one line saying how many memories the turn was given, with the paths
  in the open body rather than one row per memory. A local command draws the
  command it ran, beside the assistant twin that carries its output - the raw
  frame no longer draws a second copy of the same words. A project
  development-intent frame draws one quiet line naming the platform the CLI
  saw, with the evidence in its open body.
- **The CLI's own repository events belong to the working-tree strip, not
  the conversation.** A `vcs_state_changed` frame is the tree's own news: the
  strip above the composer says what moved for a moment - the kind and, where
  the CLI attributed it, the branch - and the tree row re-reads on the frame
  rather than at its next staleness window, so no chat row is drawn. A
  `code_change_published` frame keeps a chat row only for the four
  transitions a reader would otherwise miss - `created`, `merged`, `closed`
  and `reopened` - naming the action, the repository and the change number,
  with the URL a reader opens as the quiet tail; every other action rides the
  strip's transient line, and the PR row carries its state after the core's
  own lookup, so an edit, a comment or a checkout is not silent. A chat row's
  open lists the frame's own fields, so a fact the line does not spell is
  still reachable. **Both are events from the live window**: the CLI writes
  neither to the transcript, so a conversation reopened from it shows the
  tree and the PR row, but not the transient line.

The conversation is cut by the server into turns, and the client folds
each turn's frames into its rows; a compaction boundary draws where it
happened, collapsed, with the header's count saying how many and the mark
saying where.

**A handoff draws a row between two conversations.** When forge replaces a
session with a fresh one that continues from a brief - at a turn boundary
past 85% of its window by default, after a compaction, or when one is asked
for - the row names the pair and the reason it fired for: the fill it
crossed, or the compaction that forced it (a manual ask says so). Opened, it
carries both full session ids, each with the control that copies it - the
recovery `/resume <old-id>` needs is on the row itself - and the brief the
successor was given: the ground truth forge read off the transcript, git and
the board - including what was still running when the seat changed: the live
agents, background tasks, monitors and the session's process tree, so the
successor knows what to resume - then the judgment the handing-over session
wrote: where the work stood, the index of what it left - memory files, board
rows, PRs and the transcript - and the next actions, so the successor opens what
the note names rather than reading a retelling of it. **The brief is one
table.** Each section takes its own labelled row, and the judgment's five
headings sit on the same label column; the value takes the width the pane has
and wraps inside its own row. Under the meta sit the record's own numbers:
when the swap happened, what the cycle counted (requests, turns and prompt
tokens) and how the successor's first turns went once they have been
measured - an unmeasured successor is absent rather than a row of zeros. The
ground truth names the forge actions the session took as well as its files
and commands: one line each for the agents, tasks, crons, Slack, Gotify,
documents, reviews, System One decisions and browser pages it used, so work a
successor cannot infer from the transcript it does not hold does not vanish
with the handover. The previous
conversation stays above it, so a swap reads as a seam in the page rather
than a seat that emptied, and the old session is one `/resume` away.

**The turn that ran the handoff draws above the row.** Its first row is the
procedure forge wrote - the prompt that asks the handing-over session to run
the handoff skill - drawn under the handoff's own mark rather than as an
ordinary machinery nudge, closed on its name and open on the whole text.
Below it are the calls the turn made, the exit call among them, so the row
is the end of work the reader can read rather than a seam with nothing
above it. **That turn comes from the handoff record, not from the
successor's transcript**, because it ran in the session that left: a client
that attached after the swap, or reloaded, draws exactly what the client
that watched the swap drew, and the closed conversation above it stays with
the client that watched it.

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

**The held history settles on the newest window.** Reading back through a
seat keeps whatever the reader pulls, and the server answers pages below its
own window from the transcript with no byte budget - on the seat from #2532 a
walk to the 64th page answer pulled 55 MB of pages. So when the reader comes
back to the latest, the oldest pages are dropped until the held set fits
8 MiB, and a seat the reader has left settles on each page that lands while it
is away. Dropping is by whole page and never under
the row at the column's top edge: a page's rows share the cursor that
re-fetches it, so a row cut from inside one would be skipped. The newest page
and the subscribe seed stay whole even when they alone are over the window -
their rows name the place above themselves, and no `more` can return them.
The first page left carries the cursor that re-fetches it - the one its page
was asked with, or the newest page's own handle when the trim kept that page
whole - so scrolling back into the trimmed stretch asks for it again and the
column's top-edge pill says it is loading. Nothing is made unreachable.

## The strip

One lane of segments above the foot, in a fixed order: the browser, the
agents, the processes, the connectors, the schedules, the MCP servers, the
files, the tree, the review, the tasks, the monitors. Each segment draws
what it has and nothing else, and the strip stands whenever anything has
something to say - a running turn is one of the things it may say, not the
condition for it, because these rows are the home of what the inspector
used to hold.

**The browser segment opens the surface, not a window** (see
[browser.md](./browser.md)): its panel lists the tabs of the profile this
seat's project resolves to, show raises one of them as the full-window
takeover, hide lowers it, and the sessions keep driving the same tabs
underneath while it is closed. **It draws only while that profile has a
tab**, so a page that has never used the browser carries no browser row.

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

**The processes row is the seat's running calls.** A call the model
backgrounded and a long foreground one the CLI is tracking while it runs
both draw, led by the CLI's own description and joined to the OS walk for
the memory and pid its command names. A foreground call settles with the
turn that ran it; a backgrounded one outlives its turn and leaves when the
CLI's own registry stops naming it.

**Every cell is a read, not a computation.** The schedules row keeps the
seat's own set - a cron names the seat that created it - and its
countdowns re-derive against the page's own clock, as the monitors' ages
do; the tasks row reads the project's slice of the board; the tree row is
the seat's own working tree, the worktree for a worker and the project's
directory for a lead, and a worker a lead pointed at a tree of its own is
named by that tree - `~/Projects/nazar` beside its project's siblings -
rather than described. A tree that could not be read says so rather than
drawing a blank. The account a row chips is on the rail's footer
beside what it is costing, not on the strip: a spend-billed key shows its day,
week, month, balance and cap, and a window-billed account shows its 5h and 7d
rows, each with its percent, the reset that lifts it, and `at limit` on the
window spending the plan.

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
  trace is spent by the seat being SHOWN, or by a prompt the reader themself
  sent; a subscription alone does not spend it, and neither does a cron's, a
  peer's or forge's own prompt. An opened
  lead's page starts a died seat again the way a sleeping one is.
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
- **An account the poller has not reported**: a window-billed account keeps
  its 5h and 7d rows with `-` for a window it has no reading for, and a
  project whose account chip has not resolved draws the account it declares -
  or `account` - with `usage not reported`. The footer never vanishes, so an
  absent snapshot cannot read as forge having no account.
- **An account at its limit**: the footer still names it and states both
  windows, because a rate-limited response is a reading rather than an
  absence. The account line says `rate limited` beside the name - the dot's
  colour alone cannot, and neither can the loading state, which reads ready
  (or bailed at boot) while the rotation holds the account out. The spent
  window says `at limit` beside its
  percent, in words as well as the amber, and each window states its own reset
  in the reader's clock - `resets 23:10`, or `resets Tue 09:00` when it is
  more than a day out; a reset the clock has passed states nothing, and its
  window stops claiming to be spent. A window the provider rejects below its
  cap is marked too: the count can be a request limit, not only a token one.
  The reading lives in memory only, so a relaunch falls back to the last
  successful probe - or `-` where there is none - until the account answers
  again.

**The column says when it is still reading.** A seat whose first page is
still out draws `Reading the conversation...` in the column itself, with the
band's sweep: leaving the words to the band alone left the column reading as a
dead screen whenever the band's line was not there yet. Every other
narrative hold - a seat coming up, a command in flight, a compaction, a
handoff - stays in the composer's band, described on the composer's own page.
The one line the column keeps besides the wait is `not running`: a worker's
seat with no spawn coming is a stop, not a wait, and the band says the same
thing.

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
