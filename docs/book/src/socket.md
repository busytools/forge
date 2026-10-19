# The socket

forge serves one WebSocket, at `/socket` on the address `[server]`
`bind`:`port` names. It is served from the process that already owns the
sessions, so a client costs a listener rather than a second scheduler, a
second cron store or a second set of connectors.

It serves data and nothing else. There is no page, no markup and no
colouring on it: a client is its own program, it draws everything itself,
and what it gets from here is what the core knows. The changed files cross
as raw hunks - lines with their kinds and their line numbers, bounded and
flagged - never as anything drawn.

## Reaching it

Loopback by default. **forge is reached over loopback or a private
network and is never exposed publicly**, which is the whole reason the
socket carries no authentication: the network is what the access control
stands on, not a proxy in front of a public bind.

## The greeting

The first message a client receives is the greeting, before it has asked
for anything:

```json
{
  "kind": "greeting",
  "version": 8,
  "forge_version": "1.0.115 · abc1234",
  "forge_version_short": "1.0.115+abc1234",
  "settings": {
    "mark": null,
    "theme": null,
    "font": null,
    "browser_idle_minutes": null,
    "browser_grace_minutes": null,
    "dictate": {
      "styling": "semi_formal",
      "structure": "prose",
      "context": "general"
    }
  }
}
```

`version` is the protocol the server speaks. It is fixed rather than
negotiated, because the server changes far more slowly than a client's
visuals do. **Nothing is gated on it**: the client records the server's
number beside its own, and a difference is a fact the surface draws - the
rail footer carries both halves' build and socket numbers, and a mismatch
raises the notice beside them. Connection is never refused over a version.
**Nothing is negotiated in either direction**: the server reads no version
from a client, because nothing a client sends carries one.

Every skew sentence says what the wire can carry. This client's build and
the command that updates the stale half are always named. The server's
build is named where the greeting carried it: a server ahead carries it,
and an older one may speak a protocol from before the field existed.

`forge_version` and `forge_version_short` are the build serving the
socket, the full stamp and the short one; the header draws the short one as
its `server` build. They ride here because the greeting is the only channel
both halves are guaranteed to have, so a skew that could name only a number
would name nothing a person can act on.

`settings` carries `forge.toml`'s `[client]` keys: the three names -
`mark`, `theme` and `font` - and the idle release's pair,
`browser_idle_minutes` and `browser_grace_minutes`. They are a client's
settings rather than this server's. They
arrive on connect so a client is configured before it draws anything and
never reads `forge.toml` itself, which keeps the file the one source of
truth. A `null` is not a failure: it means the built-in.

`settings.dictate` is `[dictate]`'s three prompt axes, and it is a
DEFAULT rather than a state: a client that captures its own audio holds
the values in force per seat, so this is what its panel draws as unset
and what its reset row returns to. Each take carries the values it
normalized by.

## Subjects

Everything a client reads is addressed by a subject, and there are five:
`home`, `session <org>/<project>/<label>`, `usage`, `dictate_models` and
`extensions`.

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
{
  "kind": "command",
  "command": {
    "cancel": { "key": { "org": "Acme", "project": "proj", "label": "lead" } }
  },
  "reply_to": null
}
```

A command's variant is its name around its field bag - `Command` has 55
variants, 51 of them struct variants; the four unit variants,
`dictate_catalogue_check`, `dictate_bench_stop`, `plugin_update_all` and
`plugin_check_updates`, cross as the name alone. An update is the same
shape one level in,
`{"kind": "update", "update": {"chat_appended": {"key": ..., "msg": ...}}}`,
and 66 of `SessionUpdate`'s 70 variants are struct variants too. The other
four are why the payload is not one shape: they are unit variants and cross
as the name alone - `"catalog_loaded"`, `"cli_version_changed"`,
`"dictate_availability"` and `"accounts_changed"`.

## What a client sends

**`subscribe {what, answering, browser, declare}`** - answered with a
`snapshot` of the whole subject. The subscription this opens is the one
updates arrive on; a frame carrying `declare` opens no subscription and is
described below.

**`answering` declares whether this client can answer a prompt, and it is
off unless you say otherwise.** The core parks a turn on the reply of
whoever registered as answering, so a client that says so must be able to
show what it is handed: one counted as able to answer and unable to draw
would leave the turn waiting on a reply that never comes. A read-only
dashboard says nothing and still READS the prompts - every update reaches
every subscriber whatever its role, and no turn hangs on it, because the
core asks only the answerers whether a prompt can be answered. A client with
a dock to answer from sets this true, usually in its first subscribe, and
the connection registers with the core accordingly before it forwards
anything.

**The declaration sticks for the connection, and it only goes one way.**
A later `subscribe` that leaves `answering` false does not take it back:
the connection keeps the answering stream it already has, because the role
is what the core counts when it decides whether a prompt can be answered,
and a client that has drawn a dock once is not made an observer again by a
second message. So say it on the first subscribe and expect it to stand.

**`browser` declares whether this client can host the browser, and it is
off unless you say otherwise.** One connection holds that role at a time -
the first capable one to declare it - and every browser tool call a session
makes is routed to that connection as a `browser_ask`. **Declaring it is a
claim about what the client can DO**: a client that declares the capability
it does not have is sent asks it can only answer with a failure, and that
arrives at the far end as a session's tool call failing rather than as the
client's mistake. A second capable client changes nothing about the first -
it stays a view like any other, and the role is not an error to be second
for; it WAITS. The role moves on by itself when its holder goes: the oldest
waiter is promoted, without declaring anything again, so a client attached
and capable is never left beside a `no browser-capable client connected`
that is false about the machine. With no capable client attached at all, a
browser tool answers that named error rather than waiting for one to
appear.

**`declare` says the frame is a role DECLARE rather than a subscription,
and it is off unless you say otherwise.** A client re-states its browser
capability as it claims the role (`browser_take_role` below): the relay
answers a host down the channel a declaration created, and a connection
displaced by an earlier take is no longer registered - so the claim
re-declares first, or it would come back `false` with the click visibly
doing nothing. A declare rides a subject the connection ALREADY watches,
and it is answered with that subject's snapshot like any `subscribe` -
**but it holds nothing**: no watch entry, no "showing" count for the seat,
and no working-tree scan behind it. The client never counts the declare,
so there is nothing it could give one back with: the entry would outlive
the page that made it. Sent on a subject nothing watches yet, a
declare leaves it unwatched: the plain frame is the one that opens a
subscription.

Neither capability declaration takes the pre-attach backlog: it goes to the
first subscriber, which is whichever view attached first. A
client reads what it missed from the subject's snapshot.

**`unsubscribe {what}`** - nothing comes back, because the client asked to
stop hearing. Note that a second plain `subscribe` to one subject adds a
second entry rather than replacing the first, so **one `unsubscribe` drops
one entry**: a client that subscribed twice and unsubscribed once still
hears that subject, and needs a second `unsubscribe` to stop. It is counted
rather than flagged for a reason - two subscriptions to one seat are one
seat still being shown, and a single unsubscribe must not take the seat
out of the set a view is watching.

**`more {conversation, before, turns}`** - a page of a session's
conversation, as whole turns. Each turn carries `key` and `messages`, which is
the same shape the session snapshot's `conversation` carries; the grouping
inside a turn is the client's to decide.

**`seat_range {slot, epoch, from, to}`** - the frames a seat's stream emitted
between two numbers, for a client that read a gap in them. Every frame the
workspace emits for a seat is numbered on that seat's own counter, so a client
that reads 5 and then 7 knows exactly one frame went missing; the seat's
`header.seq` says where the stream stands and the occupant's session id beside
it is the epoch those numbers belong to. The answer is `seat_frames`, and a
range the seat's own window no longer holds is answered from the session's
transcript instead. **An `error` naming `seat_range` means read the seat
again**: a seat forge holds no session for, or one whose occupant has changed
since those numbers were minted - a repair across two epochs would splice one
conversation onto another.

**A page that cannot be answered is REFUSED rather than answered empty.** An
empty page carries `cursor: null`, which a client reads as "nothing above" and
stops asking on - so an answer the server could not give would make the seat's
history unreachable rather than merely late. An `error` naming `more` means
ask again, not that the conversation is over.

**A cursor below the held window is the exception, and it is answered either
way.** Past its oldest frame the window stops, and the page is read from the
session's own transcript on disk by the ROW the page above stopped at: the
transcript's own bytes, because the frames the CLI streams and never writes
sit between the window's rows and no arithmetic lines the two numberings up,
so a walk from any cursor converges on the same descending chain of rows.
Where that read cannot answer - no file, a row the file does not carry, one
further from the file's end than the read's cap reaches - the answer IS the
empty page, and what it ends is the history, not the seat. An `error` still
means ask again.

**A turn can carry a frame the CLI did not send.** A backgrounded task's
ending reaches a transcript as a row of its own, and a row that opens no turn
itself still lands in a later turn than the call it ends whenever something
that does open one - a delivery, a peer message, a person's next prompt - sits
between the two, which happens to about a third of them. A turn holding such a
call carries the ending as a frame too, which is what lets a client that folds
one turn at a time read it. The forged frame has no `uuid`, because the row
keeps the id the CLI minted for it and a second one would disagree; the row
stays where it is, and a fold draws nothing for it, the ending being drawn on
the call's own row.

**`key` is `null` on every turn of a transcript-derived conversation, and a
client must not key by it.** The name comes from a `Result` frame, and a
transcript holds none: its reader maps `user`, `assistant` and `system` rows
and nothing else. A client that keys its rows by `key` collapses the whole
conversation into one. It is carried because a live session does name its
turns, and what a client does with it is its own business - the cursor is a
position and is the one handle that always names a turn.

**The boundary between turns is the server's where a page has one, and it is
the only part of the fold that crosses.** How a run of tool calls groups
within a turn is a drawing decision; where one turn ends and the next begins
is what the paging contract is built on, so a page above the window's floor
never hands over half a turn. **A page below that floor can carry a stretch
inside a turn** - a running turn's older rows are exactly that - and the
server keeps what a fold needs to read it: a page never opens on a tool
result without the call that produced it, and a running turn is the whole of
what a late-attaching view can reach when the session never ends one.

**The cursor is a position, not an index.** Echo it back as `before` and do
not take it apart: above the window's floor it names the message the page's
first turn opens on, below it names the row the page stopped at by the byte
that row starts at, and what either is made of is the server's business.
`null` means there is nothing above the page it came with, and that is where
a walk backwards ends.

**`browser_answer {id, parts, error}`** - the host's answer to one
`browser_ask`, under that ask's own id. `parts` are what the tool returned,
in order; `error` is the reason the call failed, and a failed answer carries
no parts. **An image part's bytes do NOT cross here** - the part names its
`mime_type` and the bytes ride a binary frame of their own (below), because
base64 inside this JSON would pay a third again for a screenshot. Every
answer is sent by the connection that holds the browser role and by no
other; an answer naming an ask the connection was never sent is dropped with
a debug record.

**The order is part of the contract: the answer FIRST, then one frame per
image part, in the order the parts are listed.** A frame that arrives before
the answer that declares its image is a malformed pair rather than slowness,
and the ask FAILS naming that - it is not left waiting for parts nothing has
declared. A frame whose bytes cannot be taken fails every ask on that
connection waiting for an image, with the refusal as the reason: the part it
was for can never be filled, and a session reading a failure can act where a
session waiting forever cannot. **Only the refusals an image frame can be do
that** - a short header, an unknown kind, a truncated image header, an
oversized image. The dictation stream's own refusals (an oversized or uneven
audio payload) belong to the microphone and name nothing about a screenshot,
so they leave the image waits standing.

**A partial answer has no timeout of its own, and that is the contract.**
Nothing here waits out a host that stops mid-answer; the ask ends when the
host's connection goes, and the tool call fails naming that. A host that
sends an answer and then dies between frames is therefore a failure the
session reads at disconnect time, not after a clock nobody set.

**`browser_take_role`** - take the browser role from whoever holds it. The
claimant must have declared itself capable on a `subscribe` first; the server
answers with a `browser_role` frame either way, so a refused take is visible
rather than silent. The displaced holder is TOLD (the same frame with
`hosting: false`), stops reading asks, and the calls it was carrying fail as
if its connection had gone - a take-over is loud on both sides rather than
half-done.

**`document_read {session, path, reviewable}`** - one document out of a
seat's own working tree or its project's `vault_roots` in `forge.toml`,
for the client's viewer. `path` resolves against that tree, and nothing
outside the tree or those roots reads, however the path is spelled: a
`..` walk, an absolute path elsewhere and a symlink pointing out are each
refused BY NAME. `reviewable` is what a hand-off asked for - the hand-off's own
fact rather than the reader's, so it decides nothing about whether the
viewer offers the document's review - and a PDF asked reviewable is refused
with the reason rather than quietly dropped. The read is
bounded by the `[documents] max_size_mib` ceiling (`10` unless `forge.toml`
says otherwise), and a document over it is refused naming the size before
any bytes are read. **Every refusal is an `error` naming `document_read`,
with the sentence as its `why`** - missing, unreadable, outside the tree,
over the ceiling, a PDF handed reviewable, or a markdown or text kind that
is not UTF-8 - and that sentence is what a viewer draws as its own line. The
answer is a `document_read` message or that error; those are its only two.

**`file_content {session, path, sha}`** - one file's whole content at a
revision, for a review page's between-hunk expansion. The record's diff
carries bounded hunks, so the lines between them are what only this read
can supply: `sha` is the scope's own revision - a commit's tree for a
commit's scope - and `null` reads the seat's branch tip, the new side of
the all-changes diff. **The revision is resolved to a commit id before git
sees it**, so an option-shaped string is refused as a revision rather than
read as a flag. The read is bounded by the same `[documents] max_size_mib`
ceiling the document read is, and by the file's SIZE first (`git cat-file
-s`): a file inside the ceiling reads whole, and one over it answers the
lines the ceiling reaches, flagged `truncated`, rather than refusing.
**There is no `error` arm**: a read that could not run answers as a
`file_content` message with the `failed` state and the sentence for its
`why`, because a page draws that as the absence of a control rather than as
a broken file.

**Binary messages are frames, and a frame's first byte says which kind.** A
client that captures sends one dictation frame per 20 ms of speech:

| bytes | field   | value                                                   |
| ----- | ------- | ------------------------------------------------------- |
| 1     | kind    | `0` = dictation; other values reserved but `1` is taken |
| rest  | samples | i16 little-endian, mono, 16 kHz                         |

A dictation frame carries no seat. It addresses the take its own CONNECTION
started: one connection streams one take at a time and its messages are
ordered, so a frame can only arrive between its own take's start and its
stop. A frame this server cannot decode - a short header, an unknown kind, a
payload past 16 KiB - or one that arrives with no take to hold it is dropped
with a debug record and no answer, because a take's audio has no reply
channel and the take's own outcome is what a reader sees either way.

**`1` is a browser image part's bytes**, and it belongs to an answer rather
than to a take:

| bytes | field | value                                              |
| ----- | ----- | -------------------------------------------------- |
| 1     | kind  | `1` = browser image                                |
| 8     | id    | the `browser_ask`'s id, big-endian u64             |
| rest  | bytes | the image, whose mime type the answer's part named |

A client sends one per image part, **after** the answer that declares them
and in the order the parts are listed: the first frame fills the first image
part, the second the second. The id is what says which answer the bytes
belong to, so two sessions asking at once cannot be handed each other's
picture. The payload cap is 16 MiB, and the socket's own frame limit is that
cap plus one byte, set at the upgrade - so an image a shade too big is
refused by the decoder, which fails the ask it belongs to, rather than
tearing the connection down at the socket layer where the asker would only
be told its host went away.

**What a frame that cannot be delivered does depends on whether anything is
waiting for it**, and the two cases are worth telling apart when reading a
debug record. A frame this server refuses - an unknown kind, a payload past
the cap - fails every ask on that connection that is waiting for an image,
because the part it was for can never be filled. A frame that matches NO ask
in flight is a record and nothing else: one for an image part that is
already filled, or one naming an ask that was never sent on this connection.
Neither is answered - a binary frame has no reply channel - so the record is
what makes it legible.

**`dictate_stream {key, options}`** - begin a take the CLIENT captures.
The connection that sends it feeds the audio as the binary frames above,
so nothing is opened on the server; the take registers as the message is
dispatched, so the frame that follows on the same ordered connection
always finds it. `options` is the axes that client's panel was showing.
A server-side take records from forge's
machine, and its axes are the session's stored overrides.

**A take belongs to the connection that started it.** Every `dictate_*`
update about it - the start, the levels, the phases, the end - goes back to
that connection alone: no other subscriber to the seat draws its meter, its
phases or its words, and the seat's record carries no take at all. A second
`dictate_stream` for a seat whose take is live is refused with a sentence
naming the reason, and only the refused connection hears it. A connection
that goes away drops its take rather than submitting it - its reader is
gone, so nothing more is transcribed and nothing lands anywhere. What does
land sits in that reader's box, and pressing enter sends it as an ordinary
prompt every view then sees.

**`command {command, reply_to?}`** - any of the core's own commands, as
the core's own enum. `reply_to` is absent or `null` on most of them, and
the two mean the same thing - a `null` is what the field is if a client
writes it at all - because the command is fire-and-forget: its effect
arrives through the subscription, which is why a client subscribes before
it acts.

**`reply_to` is required on exactly six of them**: a worker spawn, a
despawn, the three review writes, and an idle warning (whose verdict rides
the reply - the prompt it may deliver is separate). Their outcome rides the
reply and no update carries it, so a client that omitted `reply_to` would
not be declining a reply - it would be declining to learn whether the work
happened.

**Both mismatches are refused, with a sentence naming the field**, and
refused before the command runs: a required field left off, and a
`reply_to` set on a command that has no reply to send. The second matters
for the same reason the first does - a client that set it is waiting on a
channel nothing will ever come down.

**Each of the six answers with its own type**: a spawn with a
worker-spawn reply, a despawn with a despawn result, the review writes
with a `bool` and a review set, and an idle warning with an idle verdict.
One channel cannot carry six different
answers, so the reply type follows the command. A refusal arrives in that
same reply, as the body of the answer rather than as an `error` - which is
what the six are for. A command that is _not_ one of the six refuses as
an `error` like anything else the core declines, whether or not a reply
was asked for.

**`read_call_output` answers a call's own output**: the newest lines of
the file the CLI streamed the task's output to, or a named reason there is
none. **It answers while the task still runs as well as after it ends.**
Until the ending frame arrives the path is composed from the task id the
launch frame carries - `<base>/claude-<uid>/<slug>/<session-id>/tasks/<task-id>.output`,
where `base` is the same `CLAUDE_CODE_TMPDIR` the child runs with and the
slug is a wildcard. The file itself is there from launch, so a read taken
between the launch and the command's first line comes back as an empty
tail rather than as a path the read could not find; a path that resolves
to nothing at all answers no-path. Once the task ends, the path its
`task_notification` names is the one the read uses. Nothing is parsed out
of prose on either side.

## What a client receives

- **`snapshot {subject, data}`** - a subject in full, in answer to a
  `subscribe`.
- **`update {update, seq, span}`** - one `SessionUpdate`, for whichever
  subjects the client is subscribed to. The subject decides, and a home
  subscription has **two** arms rather than one: an update a home row draws
  something of, and an update belonging to no seat at all - the service
  status, the fatal error, the plugin records, the account pool - which is a
  field of the home's own snapshot and which only a home subscription could
  have carried. Read the second arm as absent and a page keeps what it read
  at subscribe for the life of the connection. The pool is the one that reads
  as a live state rather than as an event: a card left at `0 ready, probing`
  looks like a slow probe rather than like a page that stopped listening.

  **`seq` and `span` are the frame's place in its seat's own sequence.**
  Every frame the workspace emits for a seat is numbered on that seat's
  counter, and the number is what lets a client see what it missed: a client
  that reads 5 and then 7 knows exactly one frame went missing and asks for
  it (`seat_range`), where a snapshot's `header.seq` says where a fresh
  reader joins. `seq` is the number the frame's OLDEST update was minted
  under and `span` how many of the seat's numbers it stands for - 1 for a
  frame that is one update, and n for a run of token appends the batch folded
  into the one frame they draw as. Both are absent from a frame belonging to
  no seat, and from a server older than the fields, which leaves a client
  with the frames alone.
- **`page {conversation, turns, cursor}`** - in answer to `more`.
- **`seat_frames {slot, epoch, frames}`** - in answer to `seat_range`: the
  frames numbered in `from..=to`, oldest first, each `{seq, update}` with the
  update in the same shape a live `update` carries - so a repaired frame folds
  through the same pipeline a live one does. **The range is answered from the
  seat's own window while it holds the frames, and from the session's
  transcript below that**: the transcript's own newest rows cross, numbered to
  END at the range's end, because what a frame was numbered is minted in
  memory and written down nowhere. The client resumes exactly where it was
  reading rather than asking again for a stretch that can never arrive.
- **`document_read {session, path, document_kind, bytes, reviewable,
content_hash, threads, reviews}`** - in answer to `document_read`: the
  seat it was read for, the resolved absolute path the viewer addresses by,
  the kind the read detected (`markdown`, `pdf`, `text`, or `{"other":
"<extension>"}` kept as itself), the bytes base64 - the ceiling keeps a
  document well inside the socket's frame bound - whether the DOCUMENT is
  reviewable, which is its own fact: markdown and text are, a PDF and an
  unrecognised kind are not, so the viewer's door does not depend on how the
  page was reached - the document's own hash as DECIMAL DIGITS (the same convention an
  anchor's `content_hash` crosses under), and the document's scope read:
  its comment threads and the reviews they were filed into. `session` is
  named back so a client holding two seats routes each answer to its own
  viewer; a refused read comes back as an `error` naming `document_read`,
  under the seat that asked - and so does one whose review store could not
  be read, because an answer with no comments would draw an uncommented
  document.
- **`file_content {session, path, sha, content}`** - in answer to
  `file_content`: the three names of the ask back - so a page holding two
  seats, two scopes or several files routes each answer to its own ask, and
  the scope it names is the one that asked rather than the one on screen -
  and the read under `content`: `{"state": "read", "lines": [...],
  "truncated": bool}` for a read, `{"state": "failed", "why": ...}` for one
  that could not run. There is no `error` arm.
- **`reply {reply_to, body}`** - in answer to a command that asked for one.
- **`browser_ask {id, seat, tool, args}`** - one browser tool call, sent to
  the client that holds the browser role. **This is the socket's only
  request in the direction a client answers**: `id` is what pairs it with
  the `browser_answer` that settles it, `seat` is the session whose turn is
  waiting, `tool` is upstream's own unprefixed name, and `args` is what the
  CLI sent verbatim. Nothing else pairs the two, so an ask left unanswered
  is a session's tool call waiting - a client that cannot serve it answers
  with the reason rather than with silence.
- **`browser_role {hosting}`** - whether THIS connection holds the browser
  role, sent on every change of it: when a capable declare is granted, when
  a waiter is promoted because the holder went, and when a force-take takes
  the role away (`browser_take_role`, above). A client's own browser row
  reads it to say which client drives - before this frame, "could host" and
  "does host" looked the same from the client's side.
- **`browser_seats {seats}`** - the seats that exist (every project's lead,
  plus every worker the store holds, running or not), sent to every
  browser-capable connection: as it declares, and again after a worker
  despawns. **It is for the client's profile sweep** - the client deletes
  the named browser profiles whose owning seat is not in the set, so a
  despawned worker's profiles do not outlive it - and the set crosses WHOLE
  rather than as a delta: it is authoritative at the moment it is sent, and
  the sweep that reads it is idempotent. A store that cannot be read sends
  no set at all, because a client reading "no seats" would delete every
  profile it holds.
- **`error {what, why, seat, dispatch_id}`** - `what` failed and `why`, in
  the core's own words. `seat` names the conversation a refusal belongs to
  where it belongs to one, and `dispatch_id` the dispatch the refused press
  made, so only that press hears its own refusal; both are `null` where they
  do not apply.

## What a subject carries

The whole set, so a client author can see what is reachable without
reading the code. A subject's `snapshot` carries all of it, and its
`update`s carry the pieces that change.

**`home`** is the fleet: every project, every account, and the app-level
facts a row is drawn from.

| Field                                  | What it is                                                                                                                                                                                                                                                                                                                                                                                                   |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `projects`                             | One row per project: `project` (name, org, path, sessions, `has_model`), `work` (branch, changed, gate) read at the project's own path, `rows` (the board: each one a task with its worked time, ages and marks, plus its rollup and its epic's subject), `crons`, `connectors` (this project's own gotify and slack subscription sets), `would_bind`, and `chip` - the account the row binds and its state. |
| `fleet`                                | One glance per project, for the home's own fleet row: how many seats are live against the project's cap, the queue (ready and unowned), the rows waiting on the user, and the project's own misses by name.                                                                                                                                                                                                  |
| `agents`                               | Every seat's row: slot, label, lifecycle, whether it has background work, what it is waiting on, when it was last active, why it failed if it did, `tree` (the seat's own directory, home-shortened, only when the worker runs somewhere other than its project's), and `work` (branch, changed, gate) read at that seat's OWN directory, which for a worker is its worktree and not its project.                                                                                                                             |
| `unseen`                               | The seats whose last turn finished while nobody was showing them. A mark is drawn from this, and nothing else can reconstruct it.                                                                                                                                                                                                                                                                            |
| `accounts`                             | Loading state per account, whether all of them settled, the gateway listener's ready state and port, and each account's cached usage snapshot.                                                                                                                                                                                                                                                              |
| `connectors`                           | The connector liveness: Gotify's connection, Slack's workspaces and whether the stored subscription set failed to load. **What a project is subscribed to is not here** - those sets are per project and ride the project's own row.                                                                                                                                                                         |
| `dictate`                              | Whether dictation is on, the per-model fetch and load state, and where the models land.                                                                                                                                                                                                                                                                                                                      |
| `cli_version`                          | The installed and latest `claude` versions.                                                                                                                                                                                                                                                                                                                                                                  |
| `forge_version`, `forge_version_short` | Which forge build is serving the socket. A client draws it as the `server` build beside its own `client` build.                                                                                                                                                                                                                                                                                              |
| `service_status`                       | The statuspage's last answer, `null` for healthy or unreachable.                                                                                                                                                                                                                                                                                                                                             |
| `fatal_error`                          | The last fatal error the core held, `null` when there has been none.                                                                                                                                                                                                                                                                                                                                         |

**`session <org>/<project>/<label>`** is one seat: its record, its
conversation, and what the composer is doing.

| Field                         | What it is                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `slot`                        | The seat itself.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `header`                      | The occupant's session id - the one a copy control hands out, and `null` when there is no occupant to name (nothing started, nothing connected yet, or one dropped after a sign-in or a failed connection) - the resolved model and the catalogue a picker draws from, the effort level, the permission mode, context usage, asked for on the reads that encode a subject when the seat has none, again when a turn finishes on a seat a page holds, and again when a compaction settles, a page or not (the socket issues the ask whether or not an agent is behind the seat, so where there is none it is refused and nothing reaches the CLI; the answer lands as a `context_usage_snapshot` update only where there is one) and whether a turn is in flight. `seq` is the number the seat's next frame will carry, and the occupant's session id beside it is the epoch that number belongs to: a client that attaches mid-stream reads where the stream stands, so the frames it hears from here continue the count it holds rather than looking like a gap.                                                   |
| `conversation`                | The NEWEST turns, in order, with the compaction count - up to twenty of them, the same page a `more` answers with, so a client that wants more asks for it the way it already does. (Fewer than twenty on a seat whose window the byte ceiling holds below that many turns: the window is what the page is cut from, and its front is where the walk below the floor begins.) Each turn carries `key` and `messages`, the frames the turn ran as. The live `update` stream carries those too, and differs in ways a client sees: a run of consecutive token appends inside one flush arrives there as ONE frame carrying the summed `estimated_tokens_delta`, and a frame the server forged carries no `uuid` where one the CLI sent does. A page differs the other way as well - it carries an ending for a backgrounded task's call as a frame of its own, which the stream never sends - so read these as the ones this row states rather than as the whole list. **Every path to a client carries the same bound on a frame's structured record**: a `tool_use_result` over `[server] tool_record_cap_kb` is reduced to the keys a client draws (a mutation's patch and outside-edit mark, a question's answers and notes), and arrives as a `{omitted, bytes}` marker only when that reduction is still over the cap. The transcript keeps the record whole. |
| `work`                        | The working tree as state: branch, how much changed, and whether git runs here.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `git`                         | The tree behind the row's depth, all three from the same scan: `default_branch` (which branch the tree is measured against), `worktree` (the uncommitted files, each with its path, added and removed counts and its `status` - `modified` / `added` / `deleted` / `renamed` / `copied` / `typechange` / `unmerged` / `untracked`), and `ahead` (the branch's `commit_count`, the chain itself - each commit a short `sha`, `subject`, its own `stats` and the instant it was committed in unix seconds - and the range's own `stats`). Every `stats` block carries the full totals and the busiest files up to the scan's cap of seven, each classified with the same statuses the uncommitted layer speaks; the chain is capped at the scan's walk. `null` layers are nothing to state - a clean tree, an unscanned one - and it moves with `work_changed` frames like the row above it. |
| `pr`, `closes`                | The open pull request this seat's branch is on - its number and URL, and whether it is a `draft` - and the issues it closes, which is the `PR #N -> closes #M` line a view draws under its PR row. `null` and an empty list when there is none, or when the branch is not pushed.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `diff`                        | The changed files with their raw hunks - data, never a rendering. Both layers the `work` row's state describes (`worktree` against `HEAD`, `branch_ahead` against the merge base), each `clean`, `populated` or `scan_failed`; a populated file carries its path, the old path a rename came from, its status, `binary` / `submodule` / `truncated` flags, and the carried lines with their kind, text and line numbers. Bounded per file and per read - 400 lines and 32 KiB per file, 512 KiB of carried line text over 100 files - with every cap that bites flagged. Read once per record and never pushed, so a client that wants it fresher re-reads the seat; the tree's movement arrives as `work` updates.                                                                                                                                                                        |
| `file_index`                  | Every file under the session's scan cwd, walked with the user's own gitignore preference and with the root's own `.git` dropped whether or not that preference is on, and pushed as `file_index_changed` when a walk finds it moved. The walk follows the seat's change watch: writes inside one poke become one walk, a still tree is walked at most once per five seconds, and a frame goes out only when the index differs from the one last announced.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `mcp`                         | The session's MCP servers, their status and tools, and the failure when the read did not complete.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `processes`                   | The last walk of the session's process tree, or `null` for a seat nothing has walked - a seat somebody is showing is walked once a second and its movement is pushed as a `processes_changed` update, a seat nobody holds is not walked at all, and a session ending clears it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `background_tasks`            | The CLI's background-task registry: what it reports running, each entry with the line the row leads with, the command its own call carried and the call that began it. The processes feed leads its rows with these, because a backgrounded bash is detached from claude's tree and the OS walk cannot see it for itself.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `monitors`                    | The watches the session has running.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `pending_asks`                | Every prompt the seat is holding, oldest first, the held kinds leading (a Slack draft, a browser hand-off, a document hand-off). Two questions of one batch or two calls that ran in parallel park several at once, so this is what a client that attached mid-batch reads.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `pending_ask`                 | The front of `pending_asks` - the prompt a client that draws a single ask waits on, `null` when there is none. Kept beside the list so a client reading only this one still draws the oldest ask.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `reviews`                     | The review threads and the submitted reviews, each read separately so an unreadable one is not reported as empty.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `slash_commands`, `subagents` | What the CLI last advertised: its commands and its agent-type catalogue, pushed as `slash_commands_changed` / `subagents_changed` when a turn's init (or a plugin reload, for the commands) moves them.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `state`                       | The seat's scan cwd and `queue` - the prompts still waiting in the CLI's queue, oldest first, each `{uuid, source, text}`. The queue is a fact about the seat, so it is read here as well as followed on the stream: `prompt_queued` adds a row and `prompt_lifecycle` settles it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `composer`                    | What the composer is doing: whether the session is compacting, a sign-in it is waiting on, and the push-to-talk key and mode `forge.toml` configures. **No take and no notice**: a take belongs to the connection that started it, so its meter, its phases and its words ride that connection's own updates and never the record. The asks it is answering ride `pending_asks` rather than being copied here.                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |

**`usage`** is the token/cost pool, scanned on the ask.

| Field                                | What it is                                                                                                                                                                       |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `today`, `week`, `month`, `lifetime` | One window each, holding the same two groupings: `by_model` and `by_project` rows of tokens and notional cost, plus the `total` row. Each list is sorted by cost, descending.    |
| `pricing_available`                  | Whether a price table was loaded when the report was built. `false` means every cost is a placeholder zero, which is why it crosses rather than being left to a client to infer. |

**Nothing updates this subject.** It is a scan of the session-JSONL pool,
not a feed: no update announces that a transcript's tokens moved, so a
subscription is answered once and then hears nothing. A client that wants
the current numbers asks again by subscribing again.

**`dictate_models`** is the models page's read: the pinned models, the
catalogue check, every catalogue row, and what the feed proposes adopting.
It is a subject of its own rather than a field of `home` because the rows
are the whole catalogue and only the models page draws them.

| Field                 | What it is                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enabled`             | Whether `[dictate] enabled` is set. Carried rather than inferred from an empty `in_use`, which a switched-off section and a failed preflight share.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `models_dir`          | Where the dictation models land, or `null` when the platform has no usable cache directory and none was configured.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `in_use`              | One row per model the role runs: `role`, `file`, `size`, `sha256` (the declared digest, or `null` for an installed model that publishes none), the preflight `state` (`pending`, `downloading`, `verifying`, `fetched`, `loading`, `ready`, `failed`), `facts` - what the pin declares about the checkpoint (quant, parameters, licence, runtime) - `catalogue`, the feed's entry for this file joined by file name, with the pin's byte length as the witness, when the feed carries one, and `from` / `at`, where the choice came from (a `[dictate]` config key, an installed pick, or the compiled pin) and when a runtime pick chose it. |
| `check`               | The last catalogue check, tagged on `state`: `never`, `checking`, `fresh` (`at`, `release`, `skipped`) or `unreachable` (`error`). The rows stand through an `unreachable`: a failed check costs the freshness line, not the feed.                                                                                                                                                                                                                                                                                                                                                                                                            |
| `updates`             | One entry per in-service model the feed can read against: `role`, `file`, `current` (the in-use model's own catalogue numbers, `speed_x` and `fleurs_en_wer`) and `candidates`, every comparable row ranked, each with the rule's verdict on it and which one the rule picks.                                                                                                                                                                                                                                                                                                                                                                 |
| `rows`                | The whole feed, one row per variant: display identity, parameters, licence, languages, streaming, `download` (the preferred quant and its size), `speed` (the m4-max Metal row), `wer` (FLEURS English where the feed carries it, else the entry's own headline), `kind` (which role's list the row belongs in), `url` (the entry's own page, where the feed names one) and `download_count` (the Hub's own count for a cleanup candidate, absent from the speech feed).                                                                                                                                                                      |
| `install`, `activate` | Where the last model download and the last role swap got to, tagged on `state` (`idle`, `downloading` with its bytes, `activating` with its role, `failed` with the core's own reason).                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `installed`           | Every model this machine downloaded from the feed: `variant`, `file`, `url`, `size`, `facts` and when it landed.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `bench`, `results`    | The bench in flight - its target, its tier, its clip count and the agreement so far - and every finished run, each with its metrics, its tier, its target and the corpus it scored (clips, audio seconds, digest). Only runs sharing one tier and one corpus are comparable.                                                                                                                                                                                                                                                                                                                                                                  |
| `read_aloud`          | The read-aloud set: the recordings kept here (id, duration, bytes, digest, when), whether one is being recorded right now, the passage they are read from, the terms a run scores them on, and the last write's failure when there was one.                                                                                                                                                                                                                                                                                                                                                                                                   |

**A candidate beats the model in use on both measured axes or it is not
one**: lower FLEURS English word error rate and higher m4-max Metal
wall-clock realtime factor, both from the feed's own rows. The entry must
also carry English and must not be the variant already in use. An entry
whose file name matches the pin at a different byte length has been
rebuilt, and is never a comparison. Among the entries that clear all of it,
the fastest wins; a non-commercial licence is a fact on the row rather than
a filter, because what an adoption does now is download a model onto this
machine. **Nothing is adopted without a press**: the page proposes, and the
person presses - the core only ever does what a `dictate_activate` names.

**The check's landing is the update.** `dictate_catalogue_check` starts a
fetch, and `dictate_models_changed` carries the re-read view when it
answers; a refresh due at boot lands the same way, so a page that attached
mid-check hears the result without asking again. `dictate_availability`
reaches this subject too: when the preflight finishes loading the models -
which it announces with that update alone - a page reads again rather than
drawing `pending` chips until the next check.

**`extensions`** is the Extensions page's read: the plugin inventory, the
PATH checks the scan cannot carry, and the last update run. A subject of
its own rather than a field of `home` for `dictate_models`' reason - only
that page draws any of it.

| Field        | What it is                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `snapshot`   | The disk scan, or `null` when no plugins root resolves (`CLAUDE_CONFIG_DIR` or `HOME`), which is its own state rather than an empty machine. Carries `installed` (the registry entries: id, version, scope, enabled, install path), `marketplace` (the catalog each marketplace offers), `marketplaces` (the configured sources: name, source kind, repo, clone location), `components` (per plugin: its skills, agents, commands, hook trigger events, an mcp flag, its declared lsp servers, its `available_version`, the `auto` dependency mark, and a `load_error` with its reason), `marketplace_health` (per marketplace: plugin count, load error, install-location drift), and `token_costs` (the always-on cost per plugin version, empty until a producer fetches it). |
| `lsp`        | One row per declared LSP server of an INSTALLED plugin: the plugin, the server, the command its manifest declares, and whether that command resolves on the machine's PATH. Computed here because it is a fact about the machine the socket is on.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `update_run` | The last update run or check, in flight or finished: the trigger, whether it has finished, and one row per plugin with its scope, its status (`queued`, `updating`, `updated`, `already_current`, `failed`, `skipped`, `update_available`), the versions on either side, and the failure's own words.                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |

**The read and the frames are one shape.** `plugins_inventory_updated`
embeds the `snapshot` object and `plugins_update_run_progress` /
`_finished` embed the run, so a page that took one off the stream and a
page that read it are drawing the same bytes. The `plugins_*` family
reaches this subject in full: an action's outcome, a run's rows moving, a
failed refresh, and a rollback's result. `plugins_cli_action_failed` and
`plugins_inventory_refresh_failed` carry the core's own words for a press
that ran and failed, which is what a page draws when it has nothing else.

**Two representations of one conversation cross, and they agree.** A
settled turn arrives by `more` as the messages it ran as; the turn in
flight arrives as the same CLI frames, one `update` at a time - except
that a run of consecutive token appends inside one flush crosses as ONE
frame carrying what they grew by summed, where a page's copy of the same
turn carries the CLI's own several. **A client folds both with one rule of
its own** - how a run of tool calls groups, the labels it draws and the
tail it shows - and the two meet with nothing to reconcile about their
shape, because the frames are the same shape on either side. The one thing
the server keeps is the turn boundary where a page has one - and, below the
window's floor, the call a result belongs to, so a page never opens on a
result whose call it does not carry; where a turn BEGINS is a fact about the
session, and how its work is drawn is not. Neither view is short of a token for the fold:
both draw a turn's estimate as the sum of the deltas, so the two agree on
the number where they differ in frames. **What can differ is the id, and
only for the forged frame below.**

**Not every frame on the stream is the CLI's, and for the forged kind the
id is the tell.** A prompt handed to `claude` on stdin is not echoed back,
so forge forges the user turn itself and sends it as a `chat_appended` like
any other frame: a delivery (a cron fire, a Gotify notification, a Slack
message, a peer comm), forge's own machinery (a worker kick, an
auto-continue, a board chase, a board notice), and a reader's own send,
which without it no viewer but the sender would ever see. **The folded frame
is the other kind, and its id is the CLI's**: it is the last arrival's,
because the one frame stands for the run the flush carried.

**A prompt frame carries an `origin`**, `ui` or `view`. It says who
submitted the words, is stamped where the dispatch happened rather than
sent by the client, and it is `null` on every frame the CLI sent and on
every delivery turn - so a client that ignores it draws as it did before
the field existed. `view` means a view dispatched them over the socket;
`ui` marks the core's own dispatch entry and is what a view that draws
its own words optimistically reads to skip them.

**And it carries a `source`**, the same label the queued row wears: `you`
for a person's send, and the delivery's or the machinery's own kind
otherwise. A frame's shape cannot say this - forge's machinery forges prose
as the reader's own row on the frame a person's send uses - so it is the
one field that tells a reader's words from forge's when a client decides
whether new words should bring a scrolled-up reader back to the foot.

**A forged turn carries the prompt's own `uuid`**, the id the prompt was
dispatched under. The CLI stamps the prompt's id into the transcript it
persists - on the user row of a prompt that started a turn, and inside the
`queued_command` block's `source_uuid` when a mid-turn prompt is delivered
as an attachment - so the frame and the page's later copy of the same
message agree on it, and a client reconciles the two by id. It is also what
lets a view hold the forged row while the prompt waits in the pile: the
`command_lifecycle` frames carry the same id.

**The server's fold and the client's are two readings of one conversation.**
The server folds a conversation into where its turns sit and the task
endings it holds, which is what its pages are cut on and what a carried
ending rides; the client's chat makes its own calls over the same
frames - its own unit fold, its own lifecycle carving, its own status
aggregation. The drawing is the client's, and the client's answer is the
one on screen.

## What is not here

- **Anything drawn from the diff.** The changed files and their raw hunks
  cross, bounded and flagged (the session table's `diff` row); what does
  not is any rendering of them - glyphs, colours, folding, a tree,
  highlighting. The two heavier reads a review surface wants are here now:
  one commit's own diff (`commit_diff`, asked for on demand rather than
  carried on the record) and one file's whole content at a revision
  (`file_content`, above, for a between-hunk expansion). What is still not
  here is a diff against an arbitrary base.
- **The rest of the extensions surface.** The inventory, the PATH checks,
  the last run and six actions cross (`extensions`, above: install, one
  update, update-all, a check, marketplace add and marketplace repair).
  What does not is the per-plugin details overlay and the verbs it holds -
  enable, disable, uninstall, roll back, marketplace remove and update -
  and the always-on token cost a plugin charges. Filed as its own piece of
  work (the details overlay and the rest of the plugin verbs).
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
  CLI itself draws; the name crosses and the surface does not, so a
  client that offers one is offering a command whose UI it cannot show.
- **Any rendering.** Glyphs, colours, weights, spacing, the order of a
  list and the label a row is spelled with are the client's. The test is
  whether removing a thing changes what the data IS or only how it is
  DRAWN.
- **A shipped client.** The client under `client/` speaks this
  socket, and it is what this page is for. It is a client rather than an
  instrument, and it runs from `just client-dev` rather than from an
  installed bundle, so what is missing is a released one rather than a
  consumer. The instruments are three, and they are not equal:
  `forge-protocol-client` and `forge-fold-cost`, both under
  `crates/forge-server/src/bin/` and neither installed nor shipped - the
  first by a normal build, the second only with `--features testing`,
  because it serves a fixture fleet - and the integration tests that open
  real clients against a server they start themselves.
  `forge-fold-cost` also measures: it prices one request in `ps` CPU time
  against an idle arm, on a server over a real transcript, and every arm
  prints the bytes and turns it moved so a reader can tell an arm that
  stopped seeing the conversation from one that worked.

## Today

The `forge` binary is the server: it boots the core and binds the
socket, so a running forge serves one. The client under `client/` is
what reads it.
