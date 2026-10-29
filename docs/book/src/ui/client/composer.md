# The composer

The input surface at the foot of [a session page](./session.md): one
surface whose lanes are the strip, the field, and - while the agent is
asking something - the prompt's dock, which takes the field's place
entirely rather than stacking above it. The queued prompts that pile above
it are [the queue](./queue.md)'s page. The drawing it is held against is
[web-composer.html](./web-composer.html), beside this page.

The draft is the reader's own state and this client holds it, per seat: a
draft follows the seat, comes back when the reader does, and never walks
into the seat being moved to. Nothing the box shows is a bundled default;
every list it opens is a live read.

## What it draws

| Lane | Shows | Read from |
|---|---|---|
| Strip | the running turn's figures (clock, thinking, tokens, cached) and the seat's segments - the rows the session page's strip draws | the seat's record and the board's project row |
| Field | the box: the draft, the caret, the key hints, and the controls - the mic at the row's end, `send` once there is content, `stop` while a turn runs | the host's composer store |
| Dock | the front of the seat's parked asks: a question's options, or a permission's actions, over the reader's own words row | `pending_asks` |
| Hints | one line above the box where there is something to say: a sign-in, a cancelled turn resubmitting, a suggestion | `notice`, the take, the CLI's own frames |
| Autocomplete | one popover over the box: the five lists below | the CLI's advertised commands, this seat's file index, the subagent catalogue, the bundle's emoji set, the sessions this seat can resume |
| Dictation | the take card and the panel: the axes, the models each role runs, the input device | the greeting's dictate axes; `dictate` subject |

**A repository mutation is the git segment's news.** A `vcs_state_changed`
frame from a foreground command puts its kind and, where the CLI attributed
it, the branch on the segment for a moment - "committed on main", "pushed
main", "rebased on topic" - and the tree row re-reads on the same frame; the
conversation keeps no row for it. The segment's PR row is the same surface's
state: a published change's number, draft flag and closes list move there
after the core's own lookup.

**The field's row is the whole composer when there is nothing else to
say.** Idle, it is the placeholder and the mic; typing grows the draft and
the send appears; while the turn runs the figures lead the strip lane and
the stop joins the controls. A control that has no work is absent rather
than drawn disabled.

**Every send is stamped.** The composer mints a uuid for the prompt and
sends the words under it, which is what makes the CLI emit the lifecycle
frames [the queue page](./queue.md) reads; without a stamp the CLI emits
none.

## The five lists

Five triggers, one popover shape, each row carrying the real icon for its
kind rather than a character-cell prefix and the matched span highlighted:

| Trigger | List | Source |
|---|---|---|
| `/` at the start of a draft | commands, forge's own leading, the CLI's advertised following under their own heading | forge's command table in the bundle; the CLI's `slash_commands` read for this seat |
| `@` | this seat's files, by path from the session's root | the file index walk |
| `&` | the subagent types the CLI names | the subagents catalogue |
| `:name` | emoji, by shortcode | the bundle's own set |
| `/resume ` and `/resume <partial>` | the sessions this seat can jump back to, newest first: the id, and the transcript's own title beside it | the record's `resume_sessions` read |

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

## Dictation

The mic sits at the field row's end; a take runs `ready` (the capture is
open, nothing said yet), `recording` from the first real sample, and
`transcribing` after the release, and the take card carries the counts
where this page produced the capture. Push-to-talk is `forge.toml`'s own
binding (right or left Cmd, or off), in one of three modes: a clean
press-release under 300ms is a tap, and whether a tap keeps recording or
finishes is the mode's business. The panel opens from the mic: the three
axes - styling, structure, context - the models each role runs, and the
input device. The axes are per seat and remembered on this machine, so a
reload returns what the reader chose rather than the config's value.

**The panel hangs off the input row, not the surface above it.** The strip
lane changes height with every turn, so a panel anchored to the surface would
move and re-cap as a turn starts and ends; anchored to the row, the strip
grows under it and the panel stays where it is. Its models read is held for
the connection rather than remade whenever the page hands the composer its
props again, so a record frame never blinks the rows back to "reading".

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
answers neither way. Past twenty seconds the line above the box says the
microphone did not come up, and the take the open began is left nobody's:
when it answers late it is ended, so the next press records rather than
being refused as busy.

## The states the surface can be in

| State | Draws |
|---|---|
| Idle | the field row alone, placeholder and mic |
| Typing | the draft, the send joining the controls |
| Running | the turn's figures leading the strip lane; `stop` beside the mic, and escape its key |
| A question parked | the dock's lane in place of the field |
| Sign-in needed | the box stays, with the sign-in line above it: the reader is told where, not blocked |
| A handoff | `handing this session over`, the sweep, no field |
| Compacting | `compacting this session`, the sweep, no field |
| A command in flight | `Running <name>` for the slash command the turn is still working on, the sweep |
| Waking | `Waking up agent...` while the lead's seat comes up, the sweep |
| Not running | `not running` and the seat's own reason: a worker's seat has no spawn coming, and says so |
| A take that does not come up | the box stays, with the line above it: the microphone did not come up |
| Failed | `Input disabled due to error` and what to do: quit and start forge again |

**A blocked state replaces the box entirely and always says why.** The
order the states are read in is the one a reader has to be told: a failing
seat first, then a seat nothing has started, then the more specific truth
about a turn the reader caused.

**A conversation still being read is not one of them.** The seat is up and
takes a prompt, so the box keeps its field while the column says
`Reading the conversation...` - and the words a send puts there are drawn
under that line as the reader's own row, not lost until the page lands.

**Every narrative hold is one draw.** Waking, a command in flight, a
compaction and a handoff all replace the box with one line and the same sweep
moving through its words - only the text changes - so a hold reads the same
wherever it comes from. The two states that are not progress carry no motion:
`not running` states a stop, and the error keeps its own tone and its sub.

**A machine turn is one the reader cannot type into.** A handoff replaces the
seat when its judgment turn ends, and a compaction is the CLI rewriting the
context; both take the field, the send and the mic away and name themselves in
the line. The push-to-talk key begins nothing there, but a take already running
still ends on it: the box draws no row to end one from, and Escape would throw
the words away. The draft is the reader's and stays on the seat, so the words
come back with the field when the machine turn ends.

**A parked prompt outranks the machine line.** Both machine turns are ordinary
tool-using turns - the judgment turn writes memory files and stops processes -
so they raise permission prompts, and this box is the only surface that answers
them. The dock draws, the line stands down, and the turn carries on once the
prompt is answered; the alternative is a seat waiting for an answer nobody can
give. A compaction still running when a handoff ends is read back off the
record, so the box does not come alive under it.

## The two widths

The surface is drawn and checked on the session page's column, at 1600 and
at 430. Below the width that holds the strip's figures and chips together
the chips take a lane of their own that scrolls sideways, and the dock's
rows keep their order. Every control a finger reaches takes a 44px target
under a coarse pointer, keyed on the pointer rather than the width - the
phone has no push-to-talk key, so the mic is the only way into dictation
there.
