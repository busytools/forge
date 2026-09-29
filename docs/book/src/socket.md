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

## How a message is tagged

**Every message is tagged on `kind`**, which sits beside the message's own
fields rather than replacing them. The bold name in the two sections below
is that tag, so the `subscribe` below is `{"kind": "subscribe", ...}`.

**The two payloads are tagged differently, and this is where a client goes
wrong.** `command` carries one of the core's own `Command` values and
`update` carries a `SessionUpdate`, and those two enums are tagged on the
variant's own name rather than on `kind`:

```json
{"kind": "command", "command": {"cancel": {"key": {"org": "Acme", "project": "proj", "label": "lead"}}}, "reply_to": null}
```

A command's variant is its name around its field bag - `Command` has 33
variants and every one is a struct variant. An update is the same shape one
level in, `{"kind": "update", "update": {"chat_appended": {"key": ..., "msg": ...}}}`,
and 51 of `SessionUpdate`'s 55 variants are struct variants too. The other
four are why the payload is not one shape: three are unit variants and
cross as the name alone, `"catalog_loaded"`, and one is a newtype, its
name around the value inside it.

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

**The declaration sticks for the connection, and it only goes one way.**
A later `subscribe` that leaves `answering` false does not take it back:
the connection keeps the answering stream it already has, because the role
is what the core counts when it decides whether a prompt can be answered,
and a client that has drawn a dock once is not made an observer again by a
second message. So say it on the first subscribe and expect it to stand.

Neither way takes the pre-attach backlog: it goes to the first subscriber,
and the view that draws the boot notice is the terminal. A client reads
what it missed from the subject's snapshot.

**`unsubscribe {what}`** - nothing comes back, because the client asked to
stop hearing. Note that a second `subscribe` to one subject adds a second
entry rather than replacing the first, so **one `unsubscribe` drops one
entry**: a client that subscribed twice and unsubscribed once still hears
that subject, and needs a second `unsubscribe` to stop. It is counted
rather than flagged for a reason - two subscriptions to one seat are one
seat still being shown, and a single unsubscribe must not take the seat
out of the set a view is watching.

**`more {conversation, before, turns}`** - a page of a session's
transcript, newest turn last, as whole turns.

**The cursor is a position, not an index.** Echo it back as `before` and
do not take it apart: it names the row the page opens on, and what it is
made of is the server's business. `null` means there is nothing above the
page it came with, and that is where a walk backwards ends.

**`command {command, reply_to?}`** - any of the core's own commands, as
the core's own enum. `reply_to` is absent or `null` on most of them, and
the two mean the same thing - a `null` is what the field is if a client
writes it at all - because the command is fire-and-forget: its effect
arrives through the subscription, which is why a client subscribes before
it acts.

**`reply_to` is required on exactly four of them**: a worker spawn, a
despawn, and the review pair. Their outcome rides the reply and no update
carries it, so a client that omitted `reply_to` would not be declining a
reply - it would be declining to learn whether the work happened.

**Both mismatches are refused, with a sentence naming the field**, and
refused before the command runs: a required field left off, and a
`reply_to` set on a command that has no reply to send. The second matters
for the same reason the first does - a client that set it is waiting on a
channel nothing will ever come down.

**Each of the four answers with its own type**: a spawn with a
worker-spawn reply, a despawn with a despawn result, and the review pair
with a `bool` and a review set. One channel cannot carry four different
answers, so the reply type follows the command. A refusal arrives in that
same reply, as the body of the answer rather than as an `error` - which is
what the four are for. A command that is *not* one of the four refuses as
an `error` like anything else the core declines, whether or not a reply
was asked for.

## What a client receives

- **`snapshot {subject, data}`** - a subject in full, in answer to a
  `subscribe`.
- **`update {update}`** - one `SessionUpdate`, for whichever subjects the
  client is subscribed to. The subject decides, and a home subscription has
  **two** arms rather than one: an update a home row draws something of, and
  an update belonging to no seat at all - the service status, the fatal
  error, the plugin records - which is a field of the home's own snapshot and
  which only a home subscription could have carried. Read the second arm as
  absent and a page keeps what it read at subscribe for the life of the
  connection.
- **`page {conversation, rows, cursor}`** - in answer to `more`.
- **`reply {reply_to, body}`** - in answer to a command that asked for one.
- **`error {what, why}`** - `what` failed and `why`, in the core's own
  words.

## What a subject carries

The whole set, so a client author can see what is reachable without
reading the code. A subject's `snapshot` carries all of it, and its
`update`s carry the pieces that change.

**`home`** is the fleet: every project, every account, and the app-level
facts a row is drawn from.

| Field | What it is |
|---|---|
| `projects` | One row per project: `project` (name, org, path, sessions, `has_model`), `work` (branch, changed, gate), `tasks`, `crons`, `would_bind`, and `chip` - the account the row binds and its state. |
| `agents` | Every seat's row: slot, label, lifecycle, whether it has background work, what it is waiting on, when it was last active, why it failed if it did, and the seat's peer-coordination counters - the numbers its activity badge is drawn from. |
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
| `conversation` | The transcript's messages, oldest first, with the compaction count. These are the CLI's own frames, which is what the live `update` stream carries too. |
| `work` | The working tree as state: branch, how much changed, and whether git runs here. |
| `pr`, `closes` | The open pull request this seat's branch is on - its number and URL - and the issues it closes, which is the `PR #N -> closes #M` line the inspector draws. `null` and an empty list when there is none, or when the branch is not pushed. |
| `file_index` | Every file under the session's scan cwd, walked with the user's own gitignore preference. |
| `mcp` | The session's MCP servers, their status and tools, and the failure when the read did not complete. |
| `processes` | The last walk of the session's process tree, or `null` for a seat nothing has walked. Taken on the reads that encode a subject, so it is never older than the walk's own window. |
| `background_tasks` | The CLI's background-task registry: what it reports running, each entry with the line the row leads with and the command its own call carried. The processes feed leads its rows with these, because a backgrounded bash is detached from claude's tree and the OS walk cannot see it for itself. |
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

**Two representations of one conversation cross, and they are for
different halves of it.** A settled turn is drawn from the FOLD: `more`
returns whole turns as the server folded them, and a client that wants
history should draw those units and keep its own expansion state. The turn
in flight is drawn from the FRAMES: `update`s carry the CLI's own messages
as they arrive, and a client renders those without regrouping them,
because the fold is the server's and a client's own grouping would differ
from the units the same turn becomes. When that turn settles, its units
arrive by `more` and replace what the frames were drawing. The fold is
never something a client ports.

## What is not here

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
- **forge's own command table.** The commands forge handles itself, and
  what each one does, live on the server: `forge_commands` is a read on the
  view surface, and both views that exist take the table from there rather
  than carrying a copy. It is not a wire subject, so a client has to know
  the set it is offering - and the names it offers on top of that are the
  CLI's, from `slash_commands`. A client that hardcodes its own copy should
  expect it to fall behind the core's command enum, because the two are the
  same list and only one of them is generated from the code.
- **The CLI's own surfaces.** `slash_commands` carries the names the CLI
  advertises, `/config` among them. Some of those names open a dialog the
  CLI draws in a terminal; the name crosses and the surface does not, so a
  client that offers one is offering a command whose UI it cannot show.
- **The dictation device catalog.** What crosses is the input a pick has
  already moved this process to. The list of devices to pick FROM does
  not: enumerating them is a blocking walk that trips a microphone check,
  and what it names is the machine it ran on rather than the session, so
  it stays with whoever captures. A client enumerates its own; forge's own
  list stays with the terminal it captures in.
- **A live `accounts` read.** The home's account rows are the server's own
  poller's answer, and no update announces a new one: the pool is written
  with nothing emitted, so a subscriber's bars and loading state stand as
  it read them until it subscribes again. It is scan-shaped like `usage`,
  and the answer is the same - ask again rather than wait for a stream.
- **Any rendering.** Glyphs, colours, weights, spacing, the order of a
  list and the label a row is spelled with are the client's. The test is
  whether removing a thing changes what the data IS or only how it is
  DRAWN.
- **A shipped client.** No installed binary consumes this socket. The two
  things that speak it are test instruments: `forge-protocol-client` under
  `crates/forge-server/src/bin/`, built by a normal build and neither
  installed nor shipped, and the integration tests that open real clients
  against a server they start themselves.

## Today

The terminal starts the server and binds the socket, so a running forge
serves one. The web view that used to serve pages on this port is parked
while the client is built; nothing serves it in the meantime.
