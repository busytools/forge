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

Everything a client reads is addressed by a subject, and there are three:
`home`, `session <org>/<project>/<label>`, and `usage`.

A seat nobody has started is an answer rather than a silence. Subscribing
to one that is not there comes back as an `error` saying so, so a client
never draws an empty snapshot as a broken page.

## What a client sends

**`subscribe {what, answering}`** - answered with a `snapshot` of the whole
subject. The subscription this opens is the one updates arrive on.

**`answering` declares whether this client can answer a prompt, and it is
off unless you say otherwise.** The core parks a turn on the reply of
whoever registered as answering, so a client that does not say so is never
handed a prompt it would have to show - a read-only dashboard cannot hang
a turn by ignoring one. A client with a dock to answer from sets it true
in its first subscribe, and the connection registers with the core
accordingly before it forwards anything.

Neither way takes the pre-attach backlog: it goes to the first subscriber,
and the view that draws the boot notice is the terminal. A client reads
what it missed from the subject's snapshot.

**`unsubscribe {what}`** - nothing comes back, because the client asked to
stop hearing. Note that a second `subscribe` to one subject adds a second
entry rather than replacing the first, so one `unsubscribe` drops both;
subscribe once per subject, or unsubscribe as many times as you subscribed.

**`more {conversation, before, turns}`** - a page of a session's
transcript, newest turn last, as whole turns.

**The cursor is a position, not an index.** Echo it back as `before` and
do not take it apart: it names the row the page opens on, and what it is
made of is the server's business. `null` means there is nothing above the
page it came with, and that is where a walk backwards ends.

**`command {command, reply_to?}`** - any of the core's own commands, as
the core's own enum. For most of them `reply_to` is optional: omit it and
the command is fire-and-forget, because its effect arrives through the
subscription, which is why a client subscribes before it acts.

**Four commands require it**: a worker spawn, a despawn, and the review
pair. Their outcome rides the reply and no update carries it, so a client
that omitted `reply_to` would not be declining a reply - it would be
declining to learn whether the work happened. Omitting it is refused, with
a sentence naming the field.

**Each of the four answers with its own type**: a spawn with a
worker-spawn reply, a despawn with a despawn result, and the review pair
with a `bool` and a review set. One channel cannot carry four different
answers, so the reply type follows the command. A refusal arrives in that
same reply, as the body of the answer rather than as an `error` - which is
what the four are for. A command that is *not* one of the four refuses as
an `error` like anything else the core declines, whether or not a reply
was asked for.

## What a client receives

- `snapshot` - a subject in full, in answer to a `subscribe`.
- `update` - one `SessionUpdate`, for whichever subjects the client is
  subscribed to. The subject decides: a home subscriber hears a session's
  updates only when they change something a home row shows.
- `page` - in answer to `more`, with its cursor.
- `reply` - in answer to a command that asked for one.
- `error` - `what` failed and `why`, in the core's own words.

## What a subject carries

The whole set, so a client author can see what is reachable without
reading the code. A subject's `snapshot` carries all of it, and its
`update`s carry the pieces that change.

**`home`** is the fleet: every project, every account, and the app-level
facts a row is drawn from.

| Field | What it is |
|---|---|
| `projects` | One row per project: `project` (name, org, path, sessions, `has_model`), `work` (branch, changed, gate), `tasks`, `crons`, `would_bind`, and `chip` - the account the row binds and its state. |
| `agents` | Every seat's row: slot, label, lifecycle, whether it has background work, what it is waiting on, when it was last active, and why it failed if it did. |
| `unseen` | The seats whose last turn finished while nobody was showing them. A mark is drawn from this, and nothing else can reconstruct it. |
| `accounts` | Loading state per account, whether all of them settled, the gateway listener's ready state and port, each account's cached usage snapshot, and the org views with budget and unusable reasons. |
| `plugins` | Every remembered plugin update, latest write per installed entry. |
| `workers` | The live workers per project, with their charters and slots. |
| `connectors` | Gotify's connection and subscriptions, Slack's workspaces, subscriptions and load failure. |
| `dictate` | Whether dictation is on, the per-model fetch and load state, where the models land, and the device a pick has moved to. |
| `cli_version` | The installed and latest `claude` versions. |
| `forge_version`, `forge_version_short` | Which forge build is serving the socket. A client draws these rather than its own version. |
| `service_status` | The statuspage's last answer, `null` for healthy or unreachable. |
| `fatal_error` | The last fatal error the core held, `null` when there has been none. |

**`session <org>/<project>/<label>`** is one seat: its record, its
conversation, and what the composer is doing.

| Field | What it is |
|---|---|
| `slot` | The seat itself. |
| `header` | The resolved model and the catalogue a picker draws from, the effort level, the permission mode, context usage, and whether a turn is in flight. |
| `conversation` | The transcript's messages, oldest first, with the compaction count. |
| `work` | The working tree as state: branch, how much changed, and whether git runs here. |
| `file_index` | Every file under the session's scan cwd, walked with the user's own gitignore preference. |
| `mcp` | The session's MCP servers, their status and tools, and the failure when the read did not complete. |
| `processes` | The last walk of the session's process tree, or `null` for a seat nothing has walked. Taken on the reads that encode a subject, so it is never older than the walk's own window. |
| `monitors` | The watches the session has running. |
| `pending_ask` | The prompt the seat is waiting on, `null` when there is none. |
| `reviews` | The review threads and the submitted reviews, each read separately so an unreadable one is not reported as empty. |
| `slash_commands`, `subagents` | What the CLI last advertised: its commands and its agent-type catalogue. |
| `state` | The seat's scan cwd and what it dictates with, where it has overridden the defaults. |
| `composer` | What the composer is doing: a take in flight with its meter and phase, the line a finished take left, whether the session is compacting, and a sign-in it is waiting on. The ask it is answering rides `pending_ask` rather than being copied here. |

**`usage`** is the token/cost pool behind a `/usage` view, scanned on the
ask.

| Field | What it is |
|---|---|
| `today`, `week`, `month`, `lifetime` | One window each, holding the same two groupings: `by_model` and `by_project` rows of tokens and notional cost, plus the `total` row. Each list is sorted by cost, descending. |
| `pricing_available` | Whether a price table was loaded when the report was built. `false` means every cost is a placeholder zero, which is why it crosses rather than being left to a client to infer. |

**Nothing updates this subject.** It is a scan of the session-JSONL pool,
not a feed: no update announces that a transcript's tokens moved, so a
subscription is answered once and then hears nothing. A client that wants
the current numbers asks again by subscribing again.

- **The diff.** A session's working tree arrives as state - its branch and
  how much changed - and not as a diff. A full diff is a heavier read, and
  it is a surface of its own.
- **A monitor's output tail.** A `MonitorRecord` carries the path the
  watched command writes to, and that path is on the server's machine: the
  live tail is not reachable over this socket. It is a deliberate gap
  rather than an oversight, and a small one - the finished output lands in
  the conversation like any other tool result, which the transcript does
  carry. A client that wants the running tail has to be on the machine.
- **The extensions surface.** Only the update records cross, above.
  Installing, updating, rolling back and repairing a plugin are the
  `claude plugin` CLI, and the page that drives them reads its inventory
  from the config dir rather than through the view surface - so neither the
  inventory nor the actions are here. Filed as its own piece of work.
- **The emoji set.** Which shortcodes exist is the typeahead's own
  business, so a client carries its own set rather than being handed one.
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
