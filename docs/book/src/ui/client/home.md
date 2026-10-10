# Home

The client's first page: the fleet - one row per project `forge.toml`
declares, carrying the strongest state among its seats, its tree, its
counts and its named misses. Nothing is mixed: a project's own world opens
as its [board](./board.md), and the home stays the glance. It is drawn from
one snapshot of the `home` subject, and redrawn from the update stream after
it - the 30-second chase sweep announces the board, so time-derived marks
move here too. The drawing it is held against is
[web-home.html](./web-home.html), beside this page.

The app opens here whenever it has an address that answers - the one it last
connected to - and on the [connect screen](./connect.md) when it has none,
because its only input is the server URL. A deep link is the exception: it
keeps the page it was addressed at and is handed the connection, since a
session URL is how a seat stays reachable. Until a server answers there is
nothing to draw, and the client never falls back to bundled data.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Stopped | the core's last fatal, above everything else: `forge stopped:` and the terminal's own words | `fatal_error` |
| Header | the brand mark, `forge`, the forge build serving the socket, the socket protocol, the claude version, the CLI's update notice, this app's own update line (or, in a browser, the build and the latest published), and the fleet totals | `ClientSettings.mark` from the greeting; `forge_version_short`; `PROTOCOL_VERSION`; `cli_version`; the shell's update check, or the served `latest.json`; `agents` and `projects` counted |
| Band | four cards: the gateway listener, the client's own address, dictation, and the account pool | `accounts.gateway`, the connection the client made, `dictate.snapshot`, `accounts.loading` |
| Fleet row | one per project: the strongest seat mark, the project, its branch and changed-file count, when it last moved (the newest write among its seats and the sessions its catalog remembers, `now`/`3h`/`2d`, or `never` for a project with neither a seat nor a session behind it), its seats, `live of cap slots · queue · on you`, the named misses, and the way into its board | `fleet` for the counts and the misses; `projects` for the tree; `agents` and `projects[].sessions` for the seats and the age |

Every cell of a fleet row is the server's own read. The mark is the
strongest of the project's seat states, drawn with the same shapes a seat
row uses; the counts are the server's (`slots` is the project's resolved
worker cap); and a miss is named rather than implied - a queue stalling with
a free slot, a worker holding no row, or a project that cannot start, which
is the refusal the row drew before the fleet. The whole row opens the
project's [board](./board.md).

**The dictation card is the way into the [models page](./dictate-models.md).**
The band's cards are facts about this forge, and that one opens `/models`,
where the pins, the catalogue check and the feed are drawn. It is a link at
rest - a chevron on the card's title line, dimmed until the pointer is on it -
because an affordance only the hover uncovers is one a touch reader cannot
find. The other three cards open nothing.

**The header draws the forge build, not this app's own version.** The header
states which forge is serving, and the client is a different program, so the
version in the shell crate's own manifest would name the wrong thing.

**The row's tree cell is the project's own read.** It carries the branch
the tree is on and how much has changed in it; a count of zero draws
nothing, because an unchanged tree is what the cell already means when it
is empty. A tree that could not be read says so in the cell's place - `not
a git repository`, `its working directory is not there` - because a blank
branch beside a real project reads as a tree with nothing to say rather
than as one that could not be asked.

## The state a row carries

The state is decided by the server and arrives decided. The client promotes
four cases of its own before drawing it - background work, a turn nobody
watched, an unanswered ask, and a failure the seat has not been shown since -
and draws it as a shape as well as a colour, so the row reads without colour:

| State | Class | Means |
|---|---|---|
| Running | `running` | a turn is in flight, or background work is running |
| Spawning | `spawning` | the subprocess is coming up |
| Idle | `idle` | alive, nothing in flight |
| Finished | `unseen` | a turn completed on a seat no client was attached to |
| Needs you | `needs` | a question or a permission prompt is waiting |
| Sign-in needed | `auth` | the bridge is waiting on `/login` |
| Failed | `failed` | setup or the run hit a fatal error |
| Turn failed | `failed` | the newest turn ended in failure, and the seat has not been opened since |
| Asleep | `asleep` | the subprocess is gone, or `/logout` took it |
| Never started | `never` | nothing has ever run in this project |

A project that cannot start does not draw a state of its own: the refusal is
one of the named misses instead, which is the same fact placed where a miss
already lives. A row that failed carries its reason as a line under it; a
failed TURN has no reason text of its own (the failure's words are in the
seat's conversation), so its line is the words `a turn failed`, and its mark
goes the moment the seat is opened.

## The states the page can be in

- **A fleet**, which is the ordinary case.
- **A stopped forge**: the core's last fatal draws above everything, in the
  terminal's own words. It reaches this page from the snapshot's recorded
  fatal, so the attach that sees it is one landing inside the wind-down
  window before the process goes; the live announcement is the fatal's own
  frame, drawn in the chat.
- **A fleet mid-change**: a row can move between any of the states above, and
  the page redraws from the stream without the reader losing their place.
- **Nothing configured**: no projects at all draws the empty state, naming
  `forge.toml` and the `[[orgs.projects]]` entry that would add one. A page
  that drew an empty list here would read as broken rather than as unset.
- **An update available**: the header names the published version when npm
  has one strictly newer than the installed CLI. Both sides have to resolve
  for it to appear at all, so a probe that answered one of them draws
  nothing rather than claiming an update it cannot see.
- **This app's own update**: the header draws `client ↑ vX available` when a
  newer release is published, checked once at launch. It is a control, where
  the CLI notice beside it is not: clicking installs the update and the line
  becomes `ready - restart to finish`, which is the click that swaps into the
  new build. On the phone the same click downloads and checks the APK and the
  line becomes `ready - install it`, which hands it to the system installer;
  tapping again re-opens that prompt from the checked file rather than
  downloading twice. A failed install draws `- update failed, retry`.
- **A browser tab**: outside the shell there is nothing to install, so the
  header draws the two facts instead - `client vX`, the build it is serving,
  and `latest vY` when the app it was served with carries a manifest naming a
  newer release. It is text, not a control: the next load carries the new
  build. A page nothing serves a manifest to - the dev server - draws the
  build alone.
- **Connected, waiting for the first read**: the door has gone and the fleet
  has not arrived. The server builds a home snapshot by reading each
  project's working tree, so the window is not instant, and it is its own
  state rather than the connect form handed back - a form here would re-render
  with the address reset and a live button, and a second Enter would open a
  second socket.
- **The home was refused**: the server turned the subscription down and its
  own words are drawn. The address is not the thing that is wrong, so the
  door is not what is shown.
- **A connection that dropped**: the page keeps what it last read and says so
  above it, and the notice stays until a fresh read lands rather than clearing
  the moment the socket reopens - the window between those two is the one
  place a reader cannot tell stale rows from current ones.
- **A forge on another protocol**: the shell draws one line above the page
  naming the command that updates the stale half and this client's build -
  and the answering build where the greeting carried it - and the page
  behind it is live. Nothing is refused over a version: the notice is the
  whole answer, drawn wherever a surface meets the skew. It is the one the
  shell stands down on at the door's route, where the door draws its own
  copy in its own column.

**The unseen mark is drawn from a read of its own.** `unseen` is the list
of seats whose last turn finished while no client was attached to them,
and nothing in the records reconstructs it: a turn that ended before a
client attached leaves nothing in the transcript to say it went
unwatched, so a page reading only the messages would draw every settled
seat as idle. The server keeps that fact and sends it; the client draws it
and computes none.

**Attached is not the same as drawn.** A client attaches to a seat when it
subscribes to it, and this client holds every seat it has visited - so no
mark is armed for those seats while the tab is connected, whether or not
one of their pages is open. The mark therefore means "no client was
attached to the seat", not "no client was looking at its page", and the
difference is the cost of holding a seat rather than re-reading it on
every return, filed as #1439.

Two things the home subject carries and this page does not draw are the
schedules a project holds and the account a row chips. Each is drawn
elsewhere: the schedules on the session page's strip, whose schedules row
keeps the seat's own set (a cron names the seat that created it), and the
account on the launchpad's account walk. Neither is a gap in this page.

## The two widths

The page is checked at **1600** and at **430**, against the drawing beside
it. At 430 the band becomes a two-column grid, the header wraps to two lines,
and a fleet row's cells flow onto second lines under the project name;
nothing overflows horizontally at either width.

The sheet's own breakpoints are at 1280, 980, 760 and 560. This page's
columns are all in the header and the band, so it has no collapse of its own
to check at the middle two.
