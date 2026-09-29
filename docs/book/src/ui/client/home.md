# Home

The client's first page: every project `forge.toml` declares, the agents
under each, the state each one is in, and what needs a person. It is drawn
from one snapshot of the `home` subject, and redrawn from the update stream
after it. The drawing it is held against is
[web-home.html](./web-home.html), beside this page.

The app opens on the [connect screen](./connect.md) rather than here,
because its only input is the server URL. Until a server answers there is
nothing to draw, and the client never falls back to bundled data.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Header | the brand mark, `forge`, the claude version, an update notice, and the fleet totals | `ClientSettings.mark` from the greeting; `cli_version`; `agents` and `projects` counted |
| Band | four cards: the gateway listener, the client's own address, dictation, and the account pool | `accounts.gateway`, the connection the client made, `dictate.snapshot`, `accounts.loading` |
| Org | one section per org, alphabetical, with a live and asleep count | `projects`, grouped by `org` |
| Row | one per agent: its state mark, its name, where it is, what it is doing, and when it last wrote | `agents` |

A row is the same five columns for a lead and for a worker, and a worker sits
indented under its project. The name is the link rather than the row, because
a row can also carry an artifact anchor and an anchor inside an anchor is not
HTML.

## The state a row carries

The state is decided by the server and arrives decided; the client draws it
and never recomputes one. It is drawn as a shape as well as a colour, so the
row reads without colour:

| State | Class | Means |
|---|---|---|
| Running | `running` | a turn is in flight, or background work is running |
| Spawning | `spawning` | the subprocess is coming up |
| Idle | `idle` | alive, nothing in flight |
| Finished | `unseen` | a turn completed on a seat this view was not showing |
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

Two things the row's middle columns need are not in the snapshot this page
draws from today: the per-row working tree, and the tasks a seat holds. Both
are recorded, with the server work that closes them, in
`client/src/wire/README.md`.

## The two widths

The page is checked at **1600** and at **430**, against the drawing beside
it. At 430 the band becomes a two-column grid, the header wraps to two lines,
and a row's `what` column wraps under its name; nothing overflows
horizontally at either width.

The sheet's own breakpoints are at 1280, 980, 760 and 560. This page's
columns are all in the header and the band, so it has no collapse of its own
to check at the middle two.
