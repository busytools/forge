# Home

The client's first page: every project `forge.toml` declares, the agents
under each, the state each one is in, and what needs a person. It is drawn
from one snapshot of the `home` subject, and redrawn from the update stream
after it. The drawing it is held against is
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
| Header | the brand mark, `forge`, the forge build serving the socket, the claude version, an update notice, and the fleet totals | `ClientSettings.mark` from the greeting; `forge_version_short`; `cli_version`; `agents` and `projects` counted |
| Band | four cards: the gateway listener, the client's own address, dictation, and the account pool | `accounts.gateway`, the connection the client made, `dictate.snapshot`, `accounts.loading` |
| Org | one section per org, alphabetical, with a live and asleep count | `projects`, grouped by `org` |
| Row | one per agent: its state mark, its name, where it is, what it is doing, and when it last wrote | `agents`, with each row's task from `projects` |

A row is the same five columns for a lead and for a worker, and a worker sits
indented under its project. The name is the link rather than the row, because
a row can also carry an artifact anchor and an anchor inside an anchor is not
HTML.

**The header draws the forge build, not this app's own version.** The header
states which forge is serving, and the client is a different program, so the
version in the shell crate's own manifest would name the wrong thing.

**The row's `where` and `what` cells are its two variable columns.** `where`
carries the branch the tree is on and how much has changed in it, from the
ROW's own `work`, which is read at that seat's own directory: a worker's row
draws its worktree's branch, and a lead's draws its project's. A count of zero
draws nothing, because an unchanged tree is what the cell already means when
it is empty. A seat forge holds no directory for draws the cell empty rather
than borrowing another seat's tree - a despawned worker's label is the one
that happens - and a project nobody has started draws the project's own read,
which is the only row with no seat behind it. `what` says one thing,
and the order it picks by is the order a reader needs them: what the seat is
waiting on a person for, else the task it holds with that task's status chip
and artifact, else why a spawn here would be refused, else why the tree could
not be read (`not a git repository`, `its working directory is not there`),
and a middot when none of those is true.

## The state a row carries

The state is decided by the server and arrives decided; the client draws it
and never recomputes one. It is drawn as a shape as well as a colour, so the
row reads without colour:

| State | Class | Means |
|---|---|---|
| Running | `running` | a turn is in flight, or background work is running |
| Spawning | `spawning` | the subprocess is coming up |
| Idle | `idle` | alive, nothing in flight |
| Finished | `unseen` | a turn completed on a seat no client was attached to |
| Needs you | `needs` | a question or a permission prompt is waiting |
| Sign-in needed | `auth` | the bridge is waiting on `/login` |
| Failed | `failed` | setup or the run hit a fatal error |
| Asleep | `asleep` | the subprocess is gone, or `/logout` took it |
| Never started | `never` | nothing has ever run in this project |

A project that cannot start draws its refusal in the row's `what` column
rather than a state of its own. A row that failed carries its reason as a
line under it.

## The states the page can be in

- **A fleet**, which is the ordinary case.
- **A fleet mid-change**: a row can move between any of the states above, and
  the page redraws from the stream without the reader losing their place.
- **Nothing configured**: no projects at all draws the empty state, naming
  `forge.toml` and the `[[orgs.projects]]` entry that would add one. A page
  that drew an empty list here would read as broken rather than as unset.
- **An update available**: the header names the published version when npm
  has one strictly newer than the installed CLI. Both sides have to resolve
  for it to appear at all, so a probe that answered one of them draws
  nothing rather than claiming an update it cannot see.
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
schedules a project holds and the account a row chips. Both belong to
surfaces that do not exist yet - the inspector's schedules section and the
launchpad's account walk - and neither is a gap in this page.

## The two widths

The page is checked at **1600** and at **430**, against the drawing beside
it. At 430 the band becomes a two-column grid, the header wraps to two lines,
and a row's `what` column wraps under its name; nothing overflows
horizontally at either width.

The sheet's own breakpoints are at 1280, 980, 760 and 560. This page's
columns are all in the header and the band, so it has no collapse of its own
to check at the middle two.
