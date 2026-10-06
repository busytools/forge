# The socket

forge serves one WebSocket, at `/socket` on the address `[web]`
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
{"kind": "greeting", "version": 6, "forge_version": "1.0.115 · abc1234", "forge_version_short": "1.0.115+abc1234", "settings": {"mark": null, "theme": null, "font": null, "dictate": {"styling": "semi_formal", "structure": "prose", "context": "general"}}}
```

`version` is the protocol the server speaks. It is fixed rather than
negotiated, because the server changes far more slowly than a client's
visuals do: a client reads a range of them and nothing outside it, and a
mismatch fails plainly rather than silently. **The check is the client's**:
the greeting carries the server's version, and a client that does not read
it says so and closes - the server cannot refuse a client that is behind,
because nothing a client sends carries the version it speaks.

**A client reads a floor, not one exact version.** A server one step back
is read rather than refused, because a client with no channel to its own
forge is worse off than one reading a shape whose read its own tests pin.
The floor is a literal in the client rather than "one below its own", so a
bump keeps it where it is until the new step's read coverage lands; below
the floor a client refuses. **Nothing is negotiated in either direction**:
the server gates nothing, and the tolerance is a range the client carries.

Every skew sentence says what the wire can carry. This client's build and
the command that updates the stale half are always named. The server's
build is named where the greeting carried it: a server ahead carries it,
and one a step back speaks a protocol from before the field existed. A
tolerated step is never silent: the surface that meets it draws the same
sentence a refusal does.

`forge_version` and `forge_version_short` are the build serving the
socket, in the two forms the header draws. They ride here because the
greeting is the only channel both halves are guaranteed to have: a client
that refuses a protocol never receives a snapshot, so a skew that could
name only a number would name nothing a person can act on.

`settings` is three of `forge.toml`'s `[web]` keys - `mark`, `theme` and
`font` - which are a client's settings rather than this server's. They
arrive on connect so a client is configured before it draws anything and
never reads `forge.toml` itself, which keeps the file the one source of
truth. A `null` is not a failure: it means the built-in.

`settings.dictate` is `[dictate]`'s three prompt axes, and it is a
DEFAULT rather than a state: a client that captures its own audio holds
the values in force per seat, so this is what its panel draws as unset
and what its reset row returns to. Each take carries the values it
normalized by.

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

A command's variant is its name around its field bag - `Command` has 37
variants and every one is a struct variant. An update is the same shape one
level in, `{"kind": "update", "update": {"chat_appended": {"key": ..., "msg": ...}}}`,
and 67 of `SessionUpdate`'s 72 variants are struct variants too. The other
five are why the payload is not one shape: four are unit variants and cross
as the name alone - `"catalog_loaded"`, `"cli_version_changed"`,
`"dictate_availability"` and `"accounts_changed"` - and one is a newtype,
its name around the value inside it.

## What a client sends

**`subscribe {what, answering, browser}`** - answered with a `snapshot` of
the whole subject. The subscription this opens is the one updates arrive on.

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

**`browser` declares whether this client can host the browser, and it is
off unless you say otherwise.** One connection holds that role at a time -
the first capable one to declare it - and every browser tool call a session
makes is routed to that connection as a `browser_ask`. **Declaring it is a
claim about what the client can DO**: a client that declares the capability
it does not have is sent asks it can only answer with a failure, and that
arrives at the far end as a session's tool call failing rather than as the
client's mistake. A second capable client changes nothing - it stays a view
like any other, and the role is not an error to be second for. The role is
handed back when the connection goes, and the next capable client takes it
by declaring it again (a reconnect does). With no capable client attached,
a browser tool answers the named error `no browser-capable client connected`
rather than waiting for one to appear.

Neither declaration takes the pre-attach backlog: it goes to the first
subscriber, and the view that draws the boot notice is the terminal. A
client reads what it missed from the subject's snapshot.

**`unsubscribe {what}`** - nothing comes back, because the client asked to
stop hearing. Note that a second `subscribe` to one subject adds a second
entry rather than replacing the first, so **one `unsubscribe` drops one
entry**: a client that subscribed twice and unsubscribed once still hears
that subject, and needs a second `unsubscribe` to stop. It is counted
rather than flagged for a reason - two subscriptions to one seat are one
seat still being shown, and a single unsubscribe must not take the seat
out of the set a view is watching.

**`more {conversation, before, turns}`** - a page of a session's
conversation, as whole turns. Each turn carries `key` and `messages`, which is
the same shape the session snapshot's `conversation` carries; the grouping
inside a turn is the client's to decide.

**A page that cannot be answered is REFUSED rather than answered empty.** An
empty page carries `cursor: null`, which a client reads as "nothing above" and
stops asking on - so an answer the server could not give would make the seat's
history unreachable rather than merely late. An `error` naming `more` means
ask again, not that the conversation is over.

**A cursor below the held window is the exception, and it is answered either
way.** Past its oldest frame the window stops, and the page is read from the
session's own transcript on disk, so a client can walk arbitrarily far back.
Where that read cannot answer - no file, a file whose rows no longer line up
with the session's numbering, one that would take more than the read's cap -
the answer IS the empty page, and what it ends is the history, not the seat.
An `error` still means ask again.

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

**The boundary between turns is the server's, and it is the only part of
the fold that crosses.** How a run of tool calls groups within a turn is a
drawing decision; where one turn ends and the next begins is what the
paging contract is built on, so a page can never hand over half a turn.

**The cursor is a position, not an index.** Echo it back as `before` and
do not take it apart: it names the message the page's first turn opens on,
and what it is made of is the server's business. `null` means there is
nothing above the page it came with, and that is where a walk backwards
ends.

**`devices`** - the inputs FORGE's machine can record from, and the
`[dictate] device` pin, for the terminal's own picker. **Asked on demand
rather than subscribed**: the walk opens the microphone stack, and a
subscription is re-read on every reconnect, so watching this would be a
permission check per connection instead of one per picker. A client that
captures its own audio does not ask: the inputs that matter are its
machine's, and it lists them itself, which also means their names are the
browser's own - blank until the origin has been allowed the microphone
once.

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
declared. A frame whose bytes cannot be taken (an unknown kind, a payload
past the cap) fails every ask on that connection waiting for an image, with
the refusal as the reason: the part it was for can never be filled, and a
session reading a failure can act where a session waiting forever cannot.

**A partial answer has no timeout of its own, and that is the contract.**
Nothing here waits out a host that stops mid-answer; the ask ends when the
host's connection goes, and the tool call fails naming that. A host that
sends an answer and then dies between frames is therefore a failure the
session reads at disconnect time, not after a clock nobody set.

**Binary messages are frames, and a frame's first byte says which kind.** A
client that captures sends one dictation frame per 20 ms of speech:

| bytes | field | value |
|---|---|---|
| 1 | kind | `0` = dictation; other values reserved but `1` is taken |
| rest | samples | i16 little-endian, mono, 16 kHz |

A dictation frame carries no seat. It addresses the take its own CONNECTION
started: one connection streams one take at a time and its messages are
ordered, so a frame can only arrive between its own take's start and its
stop. A frame this server cannot decode - a short header, an unknown kind, a
payload past 16 KiB - or one that arrives with no take to hold it is dropped
with a debug record and no answer, because a take's audio has no reply
channel and the take's own outcome is what a reader sees either way.

**`1` is a browser image part's bytes**, and it belongs to an answer rather
than to a take:

| bytes | field | value |
|---|---|---|
| 1 | kind | `1` = browser image |
| 8 | id | the `browser_ask`'s id, big-endian u64 |
| rest | bytes | the image, whose mime type the answer's part named |

A client sends one per image part, **after** the answer that declares them
and in the order the parts are listed: the first frame fills the first image
part, the second the second. The id is what says which answer the bytes
belong to, so two sessions asking at once cannot be handed each other's
picture. The payload cap is 16 MiB, and the socket's own frame limit is that
cap plus one byte, set at the upgrade - so an image a shade too big is
refused by the decoder, which fails the ask it belongs to, rather than
tearing the connection down at the socket layer where the asker would only
be told its host went away. A frame with no answer waiting for it, or with
every image part already filled, is dropped with a debug record.

**`dictate_stream {key, options}`** - begin a take the CLIENT captures.
The connection that sends it feeds the audio as the binary frames above,
so nothing is opened on the server; the take registers as the message is
dispatched, so the frame that follows on the same ordered connection
always finds it. `options` is the axes that client's panel was showing.
The terminal's own `dictate_start` is unchanged: it records from forge's
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
  error, the plugin records, the account pool - which is a field of the
  home's own snapshot and which only a home subscription could have carried.
  Read the second arm as absent and a page keeps what it read at subscribe
  for the life of the connection. The pool is the one that reads as a live
  state rather than as an event: a card left at `0 ready, probing` looks
  like a slow probe rather than like a page that stopped listening.
- **`page {conversation, turns, cursor}`** - in answer to `more`.
- **`devices {devices, configured}`** - in answer to `devices`: every input
  forge can record from, each with the id a pick sends back, the label a
  picker draws, and whether the system would pick it. **`configured` is the
  `forge.toml` pin only, not what is in force**: a pick moves the process's
  input for the rest of the run and rides the home snapshot's
  `dictate.device`, so a picker drawing `configured` as the current input is
  wrong from its first pick onward. A walk that could not enumerate comes
  back as an `error` naming `devices` instead, and that is what a view
  renders where the list would have been - the two are the request's only
  outcomes.
- **`reply {reply_to, body}`** - in answer to a command that asked for one.
- **`browser_ask {id, seat, tool, args}`** - one browser tool call, sent to
  the client that holds the browser role. **This is the socket's only
  request in the direction a client answers**: `id` is what pairs it with
  the `browser_answer` that settles it, `seat` is the session whose turn is
  waiting, `tool` is upstream's own unprefixed name, and `args` is what the
  CLI sent verbatim. Nothing else pairs the two, so an ask left unanswered
  is a session's tool call waiting - a client that cannot serve it answers
  with the reason rather than with silence.
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
| `projects` | One row per project: `project` (name, org, path, sessions, `has_model`), `work` (branch, changed, gate) read at the project's own path, `tasks`, `crons`, `connectors` (this project's own gotify and slack subscription sets), `would_bind`, and `chip` - the account the row binds and its state. |
| `agents` | Every seat's row: slot, label, lifecycle, whether it has background work, what it is waiting on, when it was last active, why it failed if it did, and `work` (branch, changed, gate) read at that seat's OWN directory, which for a worker is its worktree and not its project. |
| `unseen` | The seats whose last turn finished while nobody was showing them. A mark is drawn from this, and nothing else can reconstruct it. |
| `accounts` | Loading state per account, whether all of them settled, the gateway listener's ready state and port, each account's cached usage snapshot, and the org views with budget and unusable reasons. |
| `plugins` | Every remembered plugin update, latest write per installed entry. |
| `workers` | The live workers per project, with their charters and slots. |
| `connectors` | The connector liveness: Gotify's connection, Slack's workspaces and whether the stored subscription set failed to load. **What a project is subscribed to is not here** - those sets are per project and ride the project's own row. |
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
| `header` | The occupant's session id - the one a copy control hands out, and `null` when there is no occupant to name (nothing started, nothing connected yet, or one dropped after a sign-in or a failed connection) - the resolved model and the catalogue a picker draws from, the effort level, the permission mode, context usage, asked for on the reads that encode a subject when the seat has none, again when a turn finishes on a seat a page holds, and again when a compaction settles, a page or not (the socket issues the ask whether or not an agent is behind the seat, so where there is none it is refused and nothing reaches the CLI; the answer lands as a `context_usage_snapshot` update only where there is one) and whether a turn is in flight. |
| `conversation` | The NEWEST turns, in order, with the compaction count - the same twenty `more` answers a page with, so a client that wants more asks for it the way it already does. Each turn carries `key` and `messages`, the frames the turn ran as. The live `update` stream carries those too, and differs in ways a client sees: a run of consecutive token appends inside one flush arrives there as ONE frame carrying the summed `estimated_tokens_delta`, and a frame the server forged carries no `uuid` where one the CLI sent does. A page differs the other way as well - it carries an ending for a backgrounded task's call as a frame of its own, which the stream never sends - so read these as the ones this row states rather than as the whole list. |
| `has_dispatches` | Whether the conversation holds a sub-agent dispatch at all, anywhere in it - not only in the window `conversation` carries - pushed as `dispatches_changed` when one is made. A view deciding whether to draw a sub-agents section reads this rather than scanning the window, which would report a seat that dispatched an hour ago as one where nothing ran. |
| `work` | The working tree as state: branch, how much changed, and whether git runs here. |
| `pr`, `closes` | The open pull request this seat's branch is on - its number and URL - and the issues it closes, which is the `PR #N -> closes #M` line the inspector draws. `null` and an empty list when there is none, or when the branch is not pushed. |
| `diff` | The changed files with their raw hunks - data, never a rendering. Both layers the `work` row's state describes (`worktree` against `HEAD`, `branch_ahead` against the merge base), each `clean`, `populated` or `scan_failed`; a populated file carries its path, the old path a rename came from, its status, `binary` / `submodule` / `truncated` flags, and the carried lines with their kind, text and line numbers. Bounded per file and per read - 400 lines and 32 KiB per file, 512 KiB of carried line text over 100 files - with every cap that bites flagged. Read once per record and never pushed, so a client that wants it fresher re-reads the seat; the tree's movement arrives as `work` updates. |
| `file_index` | Every file under the session's scan cwd, walked with the user's own gitignore preference, and pushed as `file_index_changed` when a walk finds it moved. The walk follows the seat's change watch: writes inside one poke become one walk, a still tree is walked at most once per five seconds, and a frame goes out only when the index differs from the one last announced. |
| `mcp` | The session's MCP servers, their status and tools, and the failure when the read did not complete. |
| `processes` | The last walk of the session's process tree, or `null` for a seat nothing has walked - a seat somebody is showing is walked once a second and its movement is pushed as a `processes_changed` update, a seat nobody holds is not walked at all, and a session ending clears it. |
| `background_tasks` | The CLI's background-task registry: what it reports running, each entry with the line the row leads with and the command its own call carried. The processes feed leads its rows with these, because a backgrounded bash is detached from claude's tree and the OS walk cannot see it for itself. |
| `monitors` | The watches the session has running. |
| `pending_asks` | Every prompt the seat is holding, oldest first, drafts leading. Two questions of one batch or two calls that ran in parallel park several at once, so this is what a client that attached mid-batch reads. |
| `pending_ask` | The front of `pending_asks` - the prompt a client that draws a single ask waits on, `null` when there is none. Kept beside the list so a client reading only this one still draws the oldest ask. |
| `reviews` | The review threads and the submitted reviews, each read separately so an unreadable one is not reported as empty. |
| `slash_commands`, `subagents` | What the CLI last advertised: its commands and its agent-type catalogue, pushed as `slash_commands_changed` / `subagents_changed` when a turn's init (or a plugin reload, for the commands) moves them. |
| `state` | The seat's scan cwd, what it dictates with where it has overridden the defaults, and `queue` - the prompts still waiting in the CLI's queue, oldest first, each `{uuid, source, text}`. The queue is a fact about the seat, so it is read here as well as followed on the stream: `prompt_queued` adds a row and `prompt_lifecycle` settles it. |
| `composer` | What the composer is doing: whether the session is compacting, a sign-in it is waiting on, and the push-to-talk key and mode `forge.toml` configures. **No take and no notice**: a take belongs to the connection that started it, so its meter, its phases and its words ride that connection's own updates and never the record. The asks it is answering ride `pending_asks` rather than being copied here. |

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

**Two representations of one conversation cross, and they agree.** A
settled turn arrives by `more` as the messages it ran as; the turn in
flight arrives as the same CLI frames, one `update` at a time - except
that a run of consecutive token appends inside one flush crosses as ONE
frame carrying what they grew by summed, where a page's copy of the same
turn carries the CLI's own several. **A client folds both with one rule of
its own** - how a run of tool calls groups, the labels it draws and the
tail it shows - and the two meet with nothing to reconcile about their
shape, because the frames are the same shape on either side. The one thing
the server keeps is the turn boundary, which is what stops a page handing
over half a turn; where a turn BEGINS is a fact about the session, and how
its work is drawn is not. Neither view is short of a token for the fold:
both draw a turn's estimate as the sum of the deltas, so the two agree on
the number where they differ in frames. **What can differ is the id, and
only for the forged frame below.**

**Not every frame on the stream is the CLI's, and for the forged kind the
id is the tell.** A prompt handed to `claude` on stdin is not echoed back,
so forge forges the user turn itself and sends it as a `chat_appended` like
any other frame - a cron fire, a Gotify notification, a Slack message, a
peer comm, and a reader's own send, which without it no viewer but the
sender would ever see. **The folded frame is the other kind, and its id is
the CLI's**: it is the last arrival's, because the one frame stands for the
run the flush carried.

**A prompt frame carries an `origin`**, `ui` or `view`. It says who
submitted the words, is stamped where the dispatch happened rather than
sent by the client, and it is `null` on every frame the CLI sent and on
every delivery turn - so a client that ignores it draws as it did before
the field existed. `ui` means the forge process's own input handler
submitted them and has already drawn them, which is what lets the terminal
skip its own words while drawing everyone else's; nothing else reads it,
because no other view's composer is optimistic.

**A forged turn carries the prompt's own `uuid`**, the id the prompt was
dispatched under. The CLI stamps the prompt's id into the transcript it
persists - on the user row of a prompt that started a turn, and inside the
`queued_command` block's `source_uuid` when a mid-turn prompt is delivered
as an attachment - so the frame and the page's later copy of the same
message agree on it, and a client reconciles the two by id. It is also what
lets a view hold the forged row while the prompt waits in the pile: the
`command_lifecycle` frames carry the same id.

**The server's fold is not what a terminal reads.** The terminal groups a
message's blocks itself, in `forge-tui`'s `ui::message::grouping`, and the
server's fold is drawn by `forge-web`, which is parked. So a view that draws
a conversation makes those calls for itself: a monitor draws no chat row, and
a settled turn's status is aggregated. Both are rules about a drawing rather
than facts about a session.

## What is not here

- **Anything drawn from the diff.** The changed files and their raw hunks
  cross, bounded and flagged (the session table's `diff` row); what does
  not is any rendering of them - glyphs, colours, folding, a tree,
  highlighting - and the heavier reads a review surface may want beyond
  those two layers, like a per-commit history or a diff against an
  arbitrary base.
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
- **The dictation device catalog as carried state.** What a record carries
  is the input a pick has already moved FORGE's process to, which is the
  terminal's own capture. The list of devices to pick FROM is asked for on
  demand, one request for one answer (`devices`), because enumerating them
  is a blocking walk that trips a microphone check - and a record is encoded
  per request and re-sent on every reconnect, so a field would be a
  permission check per frame and per connection. A client that captures its
  own audio lists its own machine's inputs and asks for none of this.
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

The terminal starts the server and binds the socket, so a running forge
serves one. The web view that used to serve pages on this port is parked,
and the client under `client/` is what reads the socket now.
