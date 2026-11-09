# The composer

The input surface at the foot of [a session page](./session.md): one card whose top row says
what this seat is doing and whose body holds exactly one thing - the field, an ask, a take's
card, or the reason it cannot take input. The queued prompts that pile above it are [the
queue](./queue.md)'s page. The drawing it is held against is
[web-composer.html](./web-composer.html), beside this page.

The draft is the reader's own state and this client holds it, per seat: a draft follows the
seat, comes back when the reader does, and never walks into the seat being moved to. Nothing the
card shows is a bundled default; every list it opens is a live read.

## What it draws

| Part | Shows | Read from |
|---|---|---|
| Live row | the running turn's figures (mark, clock, thinking, tokens, cached, written, cost) and the seat's segment chips, one line, never wrapping; the ask that waits, and the reader's draft chip | the seat's record, the board's project row, and the segment stores |
| Body | exactly one of: the field (with its notice above), an ask's dock, a take's card, a blocked line | the host's composer store, `pending_asks`, the dictate subject |
| Menu | one popover above the card: the five lists below | the CLI's advertised commands, this seat's file index, the subagent catalogue, the bundle's emoji set, the sessions this seat can resume |
| Dialog | the dictation panel: the axes, the models each role runs, the input device | the greeting's dictate axes; `dictate` subject |

**The live row is the strip, moved onto the card.** It mounts the same segment components the
strip drew - browser, terminal, subagents, processes, connectors, schedules, MCPs, files, git,
review, tasks, monitors - in that fixed order, so a segment gaining a state gains it here for
free. A
turn's figure the record does not carry is dropped, never drawn as a zero or a dash. Chips that
do not fit collapse behind `+N`, which lists them in a panel; the row clips rather than wrapping
or scrolling sideways. A segment with something waiting - the review answer, a failed process -
takes its tone: the accent for attention, red for a failure.

**The row carries the ask too.** While an ask holds the body, a chip at the row's front names it
(the question's position, an approval, a held post, a document, a browser hand-off, a terminal
hand-off, a held vault call or a held configuration write) in the
attention tone. When the reader has words in their draft, a `draft N lines · show` chip sits on
the row: one click swaps the field into the body, the ask stays mounted and hidden behind it, and
the ask's chip brings it back. Nothing the reader had chosen in the ask - a toggled set, the
marked row - is lost to the swap.

**Every send is stamped.** The composer mints a uuid for the prompt and sends the words under
it, which is what makes the CLI emit the lifecycle frames [the queue page](./queue.md) reads;
without a stamp the CLI emits none.

## The ask's dock

While a prompt holds the body it replaces the field with its own card. A held Slack draft shows
where it would go and the words that would go out, with every mention drawn as the name the
core resolved (its id when the lookup fails), as Slack draws it, and its verbs follow what it
would do: one wording for a post, an edit, a delete, a reaction, a reaction's removal and an upload
would ask the reader to approve the wrong act. A post draws `Post` and `Do not post`; an edit
draws `Apply edit` and `Do not apply`; a delete draws `Delete` and `Keep it`; adding a reaction
draws `React` and `Do not react`, and removing one draws `Unreact` and `Do not unreact`; an
upload draws `Upload` and `Do not upload`. The destination line follows the same rule: the pill
carries the workspace and the conversation, and the segment beside it names the thread a post or
an upload replies into, or the message every other operation acts on, by its author and its
opening words, with a mention in the quote drawn as its name (its id when the lookup fails), a
link as its label or address, and bold without its stars. The people the draft tags follow as
their names. A quoted message the core could not read reads as `a thread` - `a message` for an
operation whose stamp names a message - and a tag it could not resolve draws its id, so a failed
lookup is drawn as itself rather than as a missing segment. A message that itself sits inside a
thread is not named: the read answers its thread's parent, so that card reads as `a message`
rather than naming the wrong one. A root message draws no thread segment, and a draft that tags
nobody draws no tags segment.

A third answer, send back with feedback, opens a field on the card. Enter sends, Shift+Enter is
a newline, and Escape closes the field without sending. The words reach the agent as the blocked
call's refusal, so it revises and calls again, and the revision parks as its own card; nothing
posts until the reader approves that one. The press stands the card down, a line where it stood
says the feedback is on its way, and a refusal replaces that line with its reason.

**The send-back field takes dictation.** It wears the same `DictateControl` as every other text
box, so a reader can speak the correction rather than type it. While its take runs the take's
card replaces the field in the send-back row, and the card's own way out abandons the take. A
take begun there lands its words in the send-back field, not the composer's draft behind the
card; so does a take begun with the push-to-talk key while the field is open and holds the
caret, because the key names no box of its own. A take whose field closes before its words land
keeps them in the composer's draft instead, so nothing spoken is lost. A take that never starts
says so in the dock's refusal line at its foot, because the box's own notice row is not on
screen while a prompt holds the slot.

**A held configuration write draws like a held post, with both values.** A `settings__set` or
`settings__unset` call parks every key it would change, what that key holds now and what the
write would leave; one card carries a whole batch, because a section's mutually required keys
cannot be approved apart. The two verbs are `Write it` and `Don't write`, and the dock stands
down on the answer the way a post's does. Every write is held, whether the key belongs to this
project or another.

**A held vault call draws the act, never a secret.** The card leads with the tool's own name
and the core's one-line summary, then names the item and its id, the field the call reveals or
changes, the diff with every secret value masked, and the attachment's name and size when the
call moves one. The draft carries no secret value at all, so nothing the card draws can reveal
one; the value reaches the tool's result only after an approval. A card parked while the
session is locked carries only what the call's arguments already say - the item's id, the field
or the masked diff, an attachment's name and size - because naming the item needs the keys; the
name appears on a card parked after an unlock. A locked session adds a masked
master-password field and an optional 2FA code, and `Approve` stays unavailable until the
password is entered, because the approval is what performs the unlock. The two ride only an
approval: a send-back or a decline runs nothing. Its verbs are `Approve`, `Send back with a
note` and `Decline`, and the send-back opens the same words field a held post's does.

## The five lists

Five triggers, one popover shape, each row carrying the real icon for its kind rather than a
character-cell prefix and the matched span highlighted:

| Trigger | List | Source |
|---|---|---|
| `/` at the start of a draft | commands, forge's own leading, the CLI's advertised following under their own heading | forge's command table in the bundle; the CLI's `slash_commands` read for this seat |
| `@` | this seat's files, by path from the session's root | the file index walk |
| `&` | the subagent types the CLI names | the subagents catalogue |
| `:name` | emoji, by shortcode | the bundle's own set |
| `/resume ` and `/resume <partial>` | the sessions this seat can jump back to, newest first: the id, and the transcript's own title beside it | the record's `resume_sessions` read |

**The popover is over the conversation, not a lane of the bar.** It was `position: static` in
the box, and an open menu took the band from 121px to 557px; it now hangs above the card and the
bar's height does not move with it.

**The `/resume` list is the set that command can actually resume.** The
argument has to be a whole word, so a second word opens nothing rather than
composing a line the core answers as a misuse. A lead is offered its own
project's sessions and never its workers'; a worker is offered only the runs
carrying its own label, and only from the directory its own resume runs in.
The session the seat is already on is not offered: every row is a real jump.
A pick writes the id alone, so the send is the ordinary `/resume <id>`.

A query drops the headings and leaves the order; a list with no rows opens
nothing, because a popover holding only a header reads as a list that lost
its rows rather than as a query that matched nothing. The header states
the number of matches and a list is cut at a bound on ranking work, so a
truncated list never reads as the whole list. Forge's table shadows the
CLI's row of the same name - one name, one row.

## Voice

A second control sits beside the mic when `[tts] enabled` is set: the
switch for the agent's voice on the seat this client shows. It is off by
default. A click turns it on and the seat's listener count comes back; a
second click turns it off. The control draws its state as a shape, not a
colour: a struck-through speaker when off, a plain speaker when on, and a
mark beside it while a reply is sounding.
While more than one client is listening, the switch draws the room's count
beside the icon and its title names it; the count is the seat's own answer,
seeded from the seat read on a late attach.

**Listening is a mode on the seat's agent, not a second copy of the chat.**
While a client is listening, the agent speaks the substance of each update
as it would say it in a live conversation, phrasing it for the ear and
keeping it shorter than the page, while the written chat stays the record
that carries the detail. The first client in puts a note into the session
that tells the agent the person is listening and in what register.

**Dictation and voice are different directions.** The mic is how the user
speaks to the agent: it fills the box with text and stops there. The voice
switch is how the agent's replies are heard: it turns the seat's speech on
or off for this client and does nothing else.

**Voice is per client and per seat.** Every connection on the seat receives
its clips, and this client plays them only while its switch is on. The last
client out tells the agent the person stopped when the reader turned the
switch off themselves, and when the connection that held that client goes
away: a closed client or a dropped socket empties the seat's listener set
too. A seat change pauses the switch and a return to a seat the reader had
on reconnects it, so moving between seats does not toggle the agent. A
closed client or a dropped socket turns the switch off. The clips play in
the order the engine rendered them, each sentence following the one before,
and a reconnect discards the queue rather than playing a conversation with
holes in it, then resubscribes the switch if this client still wants voice.

## Dictation

The mic sits at the body row's end; a take runs `ready` (the capture is
open, nothing said yet), `recording` from the first real sample, and
`transcribing` after the release. The take's card draws in the body's own row in every phase - a
dot while it records, a spinner while it transcribes, the clock, the level graph or the section
bar, the counts and the way out - except a take begun in the send-back box, whose card draws in
the send-back row in place of its field. There is no second take shape anywhere, no pill or
badge on the live row. Push-to-talk is `forge.toml`'s own
binding (right or left Cmd, or off), in one of three modes: a clean
press-release under 300ms is a tap, and whether a tap keeps recording or
finishes is the mode's business. The panel opens from the mic: the three
axes - styling, structure, context - the models each role runs, and the
input device. The axes are per seat and remembered on this machine, so a
reload returns what the reader chose rather than the config's value.

**The panel hangs off the input row, not the card above it.** The live row changes height with
every turn, so a panel anchored to the card would move and re-cap as a turn starts and ends;
anchored to the row, the row grows under it and the panel stays where it is. Its models read is
held for the connection rather than remade whenever the page hands the composer its props
again, so a record frame never blinks the rows back to "reading".

**On the desktop the picked input stays open for the client's whole
run**: the shell takes the hold as the page that takes typing appears and
again whenever the pick changes, so a press records from the press rather
than opening a device under it, and a take reuses the held stream instead
of paying the device's own start-up. The microphone indicator stays lit
while the hold is on - the accepted cost - and macOS asks for the
microphone as the page opens rather than at the first press. In a browser
the capture is the webview's own, opened per take as it always was.

**A held capture the device has stopped serving is opened again.** The
audio layer under the client changes on its own - Android disconnects the
app's input stream when the route moves, and a disconnected stream never
delivers again - so the hold and the press open the device again when the
held capture has gone half a second without a block. One such event does
not leave every later press recording nothing.

**A capture the device is feeding nothing says so while it is running.** The
row's chip stays `ready` and no graph draws for as long as that lasts, so past
a few seconds without a sample the take line says the capture is open and
nothing is coming through it, and that line goes the moment a sample lands.
On the phone the shell's own readers name the cause where the platform reports
one - the phone silencing this app's microphone, or the device's own mute -
and everywhere else the line asks the reader to check those and whether
another app is on the microphone.

**An open that the device never answers is reported rather than waited
on.**
The open blocks on the hardware, and a device layer that is wedged - a
permission whose prompt never surfaced, a device another process holds -
answers neither way. Past twenty seconds the line above the body says the
microphone did not come up, and the take the open began is left nobody's:
when it answers late it is ended, so the next press records rather than
being refused as busy. A device layer that fails by panicking during the
take's open is answered the same way: the shell catches it, names it on that
line (`the microphone device layer panicked during the take's open: ...`),
and writes the operation, the device and a backtrace to the client log. The
audio callback has its own guard, in the capture crate: a panic in the
pipeline body forge calls from cpal's frame is caught there before it
crosses the C boundary, and the capture stops delivering. That line is a
`tracing` record, and the shell forwards it into the client log beside its
own lines. A panic that unwinds through a foreign frame still aborts; no
catch at either layer can reach it.

**Every landing copies its words.** After a take's words land in a box - the
composer's draft, the dock's words row or the send-back field - the same text
is written to the clipboard with `navigator.clipboard.writeText`, overwriting
what was there. A page over a plain-http origin has no clipboard at all and a
live one can still refuse the write; neither costs the reader the words that
just landed, and a refusal is logged at debug.

## The states the surface can be in

| State | Draws |
|---|---|
| Idle | the field row alone when the seat has nothing to report, the live row above it otherwise |
| Typing | the draft, the send joining the row; the keys as a pointer's detail |
| Running | the turn's figures leading the live row; `stop` beside the mic, and escape its key |
| A question parked | the dock in the body, its chip on the live row, the reader's draft a chip beside it |
| An ask folded | every kind folds to one bar: the kind's mark, its subject, the count behind it, a refusal line |
| Sign-in needed | the field stays, with the sign-in line above it: the reader is told where, not blocked |
| A handoff | `handing this session over`, the sweep, no field |
| Compacting | `compacting this session`, the sweep, no field |
| A command in flight | `Running <name>` for the slash command the turn is still working on, the sweep |
| Waking | `Waking up agent...` while the lead's seat comes up, the sweep |
| Not running | `not running` and the seat's own reason: a worker's seat has no spawn coming, and says so |
| Reading | `Reading the conversation...` while the seat's first page is still out, with the sweep, no field |
| A take that does not come up | the body stays, with the line above it: the microphone did not come up, or the device layer's own named failure |
| A take that failed | the field comes back with the notice above it, no recording band and no stop control: the take produced no words, so the reader is told to re-speak rather than discover it |
| A take that landed with a gap | the words land with the notice above the field, `this is what fitted · part of the take is missing`: the take hit its cap, a segment ran out of decode budget or stopped on a repetition loop, or a segment failed after words landed, so the reader knows part of what they said did not land rather than that they stopped speaking |
| Voice on | the voice switch beside the mic in the accent: a plain speaker, with a mark beside it while a reply is sounding |
| Voice off | the same switch, a struck-through speaker and dim, meaning this client is not listening to this seat |
| Failed | `Input disabled due to error` and what to do: quit and start forge again |

**A blocked state replaces the body entirely and always says why.** The
order the states are read in is the one a reader has to be told: a failing
seat first, then a seat nothing has started, then the more specific truth
about a turn the reader caused.

**The read wait is one of them.** A seat whose first page is still out says
`Reading the conversation...` and holds the body like every other narrative
state: there is nothing to type into until the page lands, so the line takes
the box rather than sitting above a live field - a row above the field grew
the composer and shrank it again before the chat appeared. A take in flight or
a parked ask takes the body ahead of it, because both are the reader's own
surface; while the read wait holds the box the push-to-talk key begins
nothing, since the field is not on screen to receive the words. A row the column already
holds - a send carried across a reload, or one the takeover bar sent - still
draws above as the reader's own, so those words are never lost. A command the
core answers itself draws no row: its answer is the feedback.

**Every narrative hold is one draw.** Waking, reading, a command in flight, a
compaction and a handoff all replace the body with one line and the same sweep
moving through its words - only the text changes - so a hold reads the same
wherever it comes from. The two states that are not progress carry no motion:
`not running` states a stop, and the error keeps its own tone and its sub.

**A machine turn is one the reader cannot type into.** A handoff replaces the
seat when its judgment turn ends, and a compaction is the CLI rewriting the
context; both take the field, the send and the mic away and name themselves in
the line. The push-to-talk key begins nothing there, but a take already running
still ends on it: the body draws no row to end one from, and Escape would throw
the words away. The draft is the reader's and stays on the seat, so the words
come back with the field when the machine turn ends.

**A parked prompt outranks the machine line.** Both machine turns are ordinary
tool-using turns - the judgment turn writes memory files and stops processes -
so they raise permission prompts, and this card is the only surface that answers
them. The dock draws, the line stands down, and the turn carries on once the
prompt is answered; the alternative is a seat waiting for an answer nobody can
give. A compaction still running when a handoff ends is read back off the
record, so the body does not come alive under it.

## The two widths

The card is drawn and checked on the session page's column, at 1600 and at
430. At 1600 the idle band measures 83px, or 46px on a seat with nothing to
report; an open menu does not move it. At 430 it measures 121px: every control
a finger reaches takes a 44px target under a coarse pointer, keyed on the
pointer rather than the width - the phone has no push-to-talk key, so the mic
is the only way into dictation there - and the key hints stand down because a
finger has no use for them. The live row never wraps at either width; the
figures give way first, and what still does not fit is behind `+N`.
