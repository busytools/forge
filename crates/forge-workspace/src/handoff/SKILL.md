---
name: handoff
description: Use when the user asks for a handoff to the next session - "do a handoff", "hand off to the next session", "write the handoff", "prep the next session", "handoff for next time". Produces the project's rolling current-state handoff document.
argument-hint: "What will the next session be used for?"
disable-model-invocation: false
---

# Handoff

Write a handoff document so a fresh agent can continue this session's work. The handoff is a
current-state document: one per project, rewritten each time, never accumulated.

## Step 1 - Memory first (REQUIRED, before any handoff text)

Complete every learning from this session into memory BEFORE writing the handoff, so the handoff
can reference them instead of restating them:

- Session learnings, traps and corrections -> the session's project memory files. If a memory
  file covers the topic, update it in place and delete superseded narration; never append an
  "update" section under the old content.
- Cross-project patterns or workarounds -> TIL files (`til/`), no need to ask.
- Update the memory index (`MEMORY.md`) in the SAME edit as any claim it points at.

A learning that exists only in the handoff is not recorded - the handoff gets rewritten, memory
persists.

## Where the handoff goes: lead or worker

The project memory directory is shared by every session in the project, so the destination
depends on the SEAT, not only the project. Ask `agents__whoami` when that tool is available
(forge sessions only):

- **label `lead`, or no forge at all** -> `handoff.md` in the project memory directory, indexed
  in `MEMORY.md` by Step 3. This is the project's rolling state, and the lead owns it.
- **any other label (a forge worker)** -> `workers/<label>.md` in the same memory directory, and
  **never** `handoff.md`, and **no** `MEMORY.md` line. A worker's continuity is already carried
  by its own resumable session and its messages to the lead; the shared file is
  the lead's, and a worker writing it overwrites the lead's rolling state with its own.

The content and shape below are the same for both. Only the address differs.

**The project index is the lead's as well.** Step 1's "update the memory index in the same edit"
is the lead's duty; a worker records its learnings in its own file and leaves `MEMORY.md` alone,
so the index never lists a seat that has gone.

## Step 2 - Write the handoff

**Location: the session's project memory directory** - the one whose `MEMORY.md` auto-loads for
this project. **The file is `handoff.md` in that directory** for a lead; a worker writes
`workers/<label>.md` as the seat section says. Rewrite it in place each time. Never
write it to /tmp (invisible to a fresh session and wiped on reboot) and never into the
workspace's tracked tree (it is a generated doc; committing it pollutes git - the memory dir is
the gitignored home for exactly this).

**Write the continuation the next session needs, in your own shape.** There is no set of parts
or headings to fill in. Whatever shape reads best, it has to carry:

- where things stand: what is done and verified, what is in flight, what is unchanged.
- what is in flight, with the id that names it: the board row, PR, branch or session.
- the decisions still governing the work, and why - at most five, one line each; the reasons
  that would change what a successor does, not a log of everything considered.
- what you tried that failed, and what stopped it.
- the exact next action, anchored: the repo root or worktree it starts in, the stable ids that
  name the work (epic, board row, PR, branch), and the commit hash it is pinned to.

Do not restate the branch, HEAD or any PID: when forge wrote a PART 1 it captured those at the
swap, and a copy written earlier is the one that goes stale.

Point at what you recorded rather than retelling it - the memory files by name, the board rows
by id, the PR numbers, the transcript. Anything the session was expected to handle and could not
finish belongs here with the reason it stopped.

Redact sensitive information (API keys, passwords, PII).

If the user passed arguments, treat them as the next session's focus and tailor the note
accordingly.

## Step 3 - Index

Add or update the `handoff.md` line in the project memory's `MEMORY.md` in the same pass. The
line leads with a status token (SHIPPED / IN FLIGHT / BLOCKED) plus a one-line hook - the index
line is what the next session actually sees; the file is only reachable through it.

A worker writes no index line: its file is its own seat's, reached by the worker itself.

## Forge: hand over and exit

When forge hands the seat over, these steps sit on top of the ones above.

**Capture the live state.** Before writing the note, list what this session started and put each
in the note with its id and where the successor picks it up:

- every other live worker, from `agents__list`, by label and slot - a live worker takes
  `agents__send_message`; a failed one is the lead's to spawn again with `agents__spawn` and
  `resume_session: true`, reusing the charter `agents__list` reports; a restarting one is
  already being healed, so wait for its next attempt; PART 1 names `agents__list` rather than
  printing them, so the successor queries the rest;
- every subagent this session dispatched, by its task and where its transcript lives - a
  subagent cannot be resumed: the swap kills it. Name the task and transcript so the successor
  re-dispatches the work; when the transcript is gone, say so - there is nothing to re-dispatch
  from;
- every running background command or monitor, by the command and where its output goes - the
  swap stops it too, so name where it got to and how to restart it;
- every live cron, from `cron__list`, by id and next fire - each fires on its own; PART 1
  names `cron__list` rather than printing them, so the successor queries the rest;
- every open board row this session owns or is still running, from `tasks__list`, by id and
  owner - a reserved row is the user's pick; a row the successor owns is its to continue, and a
  waiting one names what it waits on; an unowned pending row takes `tasks__claim`; an unowned
  row past pending is taken with `tasks__update`; a row another seat holds is that seat's to
  move; PART 1 names `tasks__list` rather than printing them, so the successor queries the rest;
- every worktree this session created, from `git worktree list`, by path and branch - a worker
  successor cannot enter one, so name the path to read from or the move the successor can make.

**Durable state hands over; the session's own children stop.** Workers, crons, board rows and the
on-disk worktree outlive the swap and belong to the successor. Subagents, background commands
and monitors are the session's own children: the swap stops them, so name where each got to and
how to restart it. When a subagent's transcript is gone, or a command's output is gone, say so:
the successor cannot pick up where it stopped.

**Verify before you wait.** A registry row can outlive its process. Check `agents__list` before
the successor waits on any worker the note names.

**Leave nothing unfinished unrecorded.** List what this session was expected to handle and could
not finish in this turn - a blocked ask, a deferred fix, a call that was refused, a question left
open - with the reason it stopped and where its context lives. The next session starts exactly
where this one stopped; anything left out is lost when the turn ends.

**Keep the note within the brief's limit.** Step 2's shape is the instruction; the guard against
forge's trim is 400 words. Forge keeps at most 5,000 characters of the note and drops the tail,
and the exact next action and the live children are written last, so a note over that limit
loses exactly them.

**Exit through the tool.** Forge takes the note as you wrote it - there are no headings to fill
in, and your Step 2 document is the note. Write it where the seat section and Step 2 say, then
pass the same text to
`agents__handoff` as `brief` as the turn's last act. Memory first, then hand over what is
running, then the note. Do not call `agents__handoff` for any other reason - a handoff starts at
`/handoff` or at the automatic trigger (fill or compaction).
