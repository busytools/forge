# Review

One seat's reviews, addressed at
`/session/{org}/{project}/{label}/review`: the loop's state, the threads
whose code has moved out from under them, and the rounds newest first with
each turn's own round on it. The drawing it is held against is
[web-review.html](./web-review.html), beside this page.

The route is a sub-path of the seat because the seat is what a review is
keyed to: the record's review read is the seat's own branch, so reviewing a
worker's work means opening that worker's seat. The page is reached by its
URL today, and its back link names the seat it belongs to.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Header | the back link to the seat, `Review`, the branch, the round count, and the loop's state - `N replies to read`, in the warn tone while any thread waits on the reader, and `N open` | the record's `work.branch`, and the two review halves |
| Outdated | every thread whose anchored code could not be placed, drawn against the lines captured when the comment was written | the threads half |
| Rounds | newest first: the round's number, its age, its tally by state, its summary, and each member thread | the rounds half |

A round's members are the threads with any turn filed into it, so a thread
the reviewer replied on across rounds counts under each of them.

## A thread

The state chip, then the anchored path and line, then the turns: the
reviewer's read as `you` behind an accent rule, an agent's by its label
behind a plain one, and the reviewer's own turns carrying the round that
sealed them. An agent's turn carries no round, because only reviewer turns
file into rounds.

The four states are the server's own: `open`, answered by nobody yet;
`addressed`, the agent has answered and the ball is with the reader;
`resolved`, read and accepted, drawn in full like every other thread;
`outdated`, the anchored code moved too far to place. Two of those draw
differently here than in the terminal, and both are the wire's limit rather
than a choice: the terminal collapses a resolved thread to a marker and it
says why an anchor could not be placed (gone, or matching several places),
while a resolved thread is simply another thread on this page and the reason
is computed by the terminal's own resolver and never crosses the wire.

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

## The states the page can be in

- **A seat's reviews**, the ordinary case.
- **Nothing yet**: `Reading the seat...` before the first record arrives.
- **A refused seat**: the server's own sentence, drawn the way the sibling
  routes draw a refused subscription.
- **A half that failed**: the two reads fail apart on the server, so each is
  drawn apart here - a failed threads read leaves the rounds drawn, and the
  reverse - and a half whose outcome this build cannot name draws that word
  rather than being folded into a failure it is not.
- **Empty**: both halves read and nothing filed draws
  `Nothing has been reviewed on this branch yet.`; threads drafted with no
  round filed says how many are drafted.

## The seed, for now

`?seed=1` files a round through the socket in a development build, so a
scratch stack has something real to draw before the page composes its own.
It lives under `client/src/dev/` behind the DEV guard with a dynamic import,
the bundle guard asserts its marker is absent from the production build, and
it deletes when the page composes and submits its own round.

## The two widths

Checked at **1600** and at **430**. The header wraps rather than scrolling
sideways, and the rounds and threads below it are one column at either
width.

## Where the surface differs from `web-session.html`

The session page's drawing named a diff review overlay as its sixth section,
deliberately not built at the time. The surface landed as a route of its own
instead, so a review is addressable and a second reader can sit on it while
the session page draws the conversation.
