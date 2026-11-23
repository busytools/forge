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
- what you tried that failed, and what stopped it.
- the exact next action, and the file, command or row it starts at.

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

**Capture the live state.** Before writing the note, list what this session started and what the
project is still running, and put each one in the note with its id and how to resume it:

- every live worker, from `agents__list`, by label and slot - resume it with
  `agents__send_message`;
- every subagent this session dispatched, by its task and its transcript - resume it with
  `agents__spawn` and `resume_session`;
- every running background command or monitor, by the command and where its output goes -
  resume it by re-running the command;
- every live cron, from `cron__list`, by id and next fire - it fires on its own, so name it
  rather than restarting it;
- every open board row, from `tasks__list`, by id and owner - the successor picks it up from
  the row;
- every worktree, from `git worktree list`, by path and branch - the successor enters it.

**Hand over, do not stop.** A live process belongs to the successor now: do not kill it on the
way out. Stopping it throws away the run it holds, and a restarted copy is not the same work. A
process that cannot be resumed - a subagent whose transcript is gone, a command whose output
nothing can read - is named as such with the reason, so the successor is not left expecting it.

**Leave nothing unfinished unrecorded.** List what this session was expected to handle and could
not finish in this turn - a blocked ask, a deferred fix, a call that was refused, a question left
open - with the reason it stopped and where its context lives. The next session starts exactly
where this one stopped; anything left out is lost when the turn ends.

**Exit through the tool.** Forge takes the note as you wrote it - there are no headings to fill
in, and your Step 2 document is the note. Write it where the seat section and Step 2 say, then
pass the same text to
`agents__handoff` as `brief` as the turn's last act. Memory first, then hand over what is
running, then the note. Do not call `agents__handoff` for any other reason - a handoff starts at
`/handoff` or at the automatic trigger (fill or compaction).
