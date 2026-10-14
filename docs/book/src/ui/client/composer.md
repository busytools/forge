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
| Strip | the running turn's figures (clock, thinking, tokens, cached) and the seat's segments - the rows the session page's strip draws | the seat's record and the home's project row |
| Field | the box: the draft, the caret, the key hints, and the controls - the mic at the row's end, `send` once there is content, `stop` while a turn runs | the host's composer store |
| Dock | the front of the seat's parked asks: a question's options, or a permission's actions, over the reader's own words row | `pending_asks` |
| Hints | one line above the box where there is something to say: a sign-in, a cancelled turn resubmitting, a suggestion | `notice`, the take, the CLI's own frames |
| Autocomplete | one popover over the box: the four lists below | the CLI's advertised commands, this seat's file index, the subagent catalogue, the bundle's emoji set |
| Dictation | the take card and the panel: the axes, the models each role runs, the input device | the greeting's dictate axes; `dictate` subject |

**The field's row is the whole composer when there is nothing else to
say.** Idle, it is the placeholder and the mic; typing grows the draft and
the send appears; while the turn runs the figures lead the strip lane and
the stop joins the controls. A control that has no work is absent rather
than drawn disabled.

**Every send is stamped.** The composer mints a uuid for the prompt and
sends the words under it, which is what makes the CLI emit the lifecycle
frames [the queue page](./queue.md) reads; without a stamp the CLI emits
none.

## The four lists

Four triggers, one popover shape, each row carrying the real icon for its
kind rather than a character-cell prefix and the matched span highlighted:

| Trigger | List | Source |
|---|---|---|
| `/` at the start of a draft | commands, forge's own leading, the CLI's advertised following under their own heading | forge's command table in the bundle; the CLI's `slash_commands` read for this seat |
| `@` | this seat's files, by path from the session's root | the file index walk |
| `&` | the subagent types the CLI names | the subagents catalogue |
| `:name` | emoji, by shortcode | the bundle's own set |

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

**On the desktop the picked input stays open for the client's whole
run**: the shell takes the hold as the page that takes typing appears and
again whenever the pick changes, so a press records from the press rather
than opening a device under it, and a take reuses the held stream instead
of paying the device's own start-up. The microphone indicator stays lit
while the hold is on - the accepted cost - and macOS asks for the
microphone as the page opens rather than at the first press. In a browser
the capture is the webview's own, opened per take as it always was.

## The states the surface can be in

| State | Draws |
|---|---|
| Idle | the field row alone, placeholder and mic |
| Typing | the draft, the send joining the controls |
| Running | the turn's figures leading the strip lane; `stop` beside the mic |
| A question parked | the dock's lane in place of the field |
| Sign-in needed | the box stays, with the sign-in line above it: the reader is told where, not blocked |
| Compacting | `Compacting context...`, a ring, no field |
| A command in flight | `Running <name>` for the slash command the turn is still working on |
| Waking | `Waking up agent...` while the lead's seat comes up |
| Not running | `not running` and the seat's own reason: a worker's seat has no spawn coming, and says so |
| Failed | `Input disabled due to error` and what to do: quit and start forge again |

**A blocked state replaces the box entirely and always says why.** The
order the states are read in is the one a reader has to be told: a failing
seat first, then a seat nothing has started, then the more specific truth
about a turn the reader caused.

## The two widths

The surface is drawn and checked on the session page's column, at 1600 and
at 430. Below the width that holds the strip's figures and chips together
the chips take a lane of their own that scrolls sideways, and the dock's
rows keep their order. Every control a finger reaches takes a 44px target
under a coarse pointer, keyed on the pointer rather than the width - the
phone has no push-to-talk key, so the mic is the only way into dictation
there.
