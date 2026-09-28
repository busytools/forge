# The socket

forge serves one WebSocket, at `/socket` on the address `[web]`
`bind`:`port` names. It is served from the process that already owns the
sessions, so a client costs a listener rather than a second scheduler, a
second cron store or a second set of connectors.

It serves data and nothing else. There is no page, no markup, no diff and
no colouring on it: a client is its own program, it draws everything
itself, and what it gets from here is what the core knows.

## Reaching it

Loopback by default. **forge is reached over loopback or a private
network and is never exposed publicly**, which is the whole reason the
socket carries no authentication: the network is what the access control
stands on, not a proxy in front of a public bind.

## The greeting

The first message a client receives is the greeting, before it has asked
for anything:

```json
{"kind": "greeting", "version": 1, "settings": {"mark": null, "theme": null, "font": null}}
```

`version` is the protocol the server speaks. It is fixed rather than
negotiated, because the server changes far more slowly than a client's
visuals do: either a client speaks this version or it does not, and a
mismatch fails plainly.

`settings` is three of `forge.toml`'s `[web]` keys - `mark`, `theme` and
`font` - which are a client's settings rather than this server's. They
arrive on connect so a client is configured before it draws anything and
never reads `forge.toml` itself, which keeps the file the one source of
truth. A `null` is not a failure: it means the built-in.

## Subjects

Everything a client reads is addressed by a subject, and there are two:
`home`, and `session <org>/<project>/<label>`.

A seat nobody has started is an answer rather than a silence. Subscribing
to one that is not there comes back as an `error` saying so, so a client
never draws an empty snapshot as a broken page.

## What a client sends

**`subscribe {what}`** - answered with a `snapshot` of the whole subject.
The subscription this opens is the one updates arrive on.

**A subscription answers as well as watches.** A permission request or a
question delivered to a subscribed client is one that client can answer,
so a client that subscribes is the one the core waits on. A client that
only wants to watch should say so rather than leave a prompt parked.

**`unsubscribe {what}`** - nothing comes back, because the client asked to
stop hearing.

**`more {conversation, before, turns}`** - a page of a session's
transcript, newest turn last, as whole turns.

**The cursor is a position, not an index.** Echo it back as `before` and
do not take it apart: it names the row the page opens on, and what it is
made of is the server's business. `null` means there is nothing above the
page it came with, and that is where a walk backwards ends.

**`command {command, reply_to?}`** - any of the core's own commands, as
the core's own enum. Omit `reply_to` and the command is fire-and-forget:
its effect arrives through the subscription, which is why a client
subscribes before it acts.

Four commands carry an answer, and **each answers with its own type**: a
worker spawn with a worker-spawn reply, a despawn with a despawn result,
and the review pair with a `bool` and a review set. One channel cannot
carry four different answers, so the reply type follows the command.
Refusals are replies too: a command that could not be carried out answers
in its own type, not with an `error`.

## What a client receives

- `snapshot` - a subject in full, in answer to a `subscribe`.
- `update` - one `SessionUpdate`, for whichever subjects the client is
  subscribed to. The subject decides: a home subscriber hears a session's
  updates only when they change something a home row shows.
- `page` - in answer to `more`, with its cursor.
- `reply` - in answer to a command that asked for one.
- `error` - `what` failed and `why`, in the core's own words.

## What is not here

- **The diff.** A session's working tree arrives as state - its branch and
  how much changed - and not as a diff. A full diff is a heavier read, and
  it is a surface of its own.
- **Any rendering.** Glyphs, colours, weights, spacing, the order of a
  list and the label a row is spelled with are the client's. The test is
  whether removing a thing changes what the data IS or only how it is
  DRAWN.
- **The client itself.** Nothing in the tree consumes this socket yet. The
  one thing that speaks it is `forge-protocol-client` under
  `crates/forge-server/src/bin/`, which is a test instrument: it is built
  by a normal build and is not installed or shipped.

## Today

The terminal starts the server and binds the socket, so a running forge
serves one. The web view that used to serve pages on this port is parked
while the client is built; nothing serves it in the meantime.
