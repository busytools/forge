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
  by its own resumable session, its board row and its messages to the lead; the shared file is
  the lead's, and a worker writing it overwrites the lead's rolling state with its own.

The content and shape below are the same for both. Only the address differs.

## Step 2 - Write the handoff

**Location: the session's project memory directory** - the one whose `MEMORY.md` auto-loads for
this project. The file is `handoff.md` in that directory; rewrite it in place each time. Never
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

## Forge: close out and exit

When forge hands the seat over, three things happen on top of the steps above.

**Close out what is running.** Anything this session started and did not finish must not outlive
it: a live sub-agent, a background command, a monitor, a process it spawned. For each one,
record in memory what it is and how to resume it - the command, the id it ran under, where its
output goes - then stop it. A successor that finds half-finished work and no record of the
process that owns it cannot tell the two apart.

**Leave nothing unfinished unrecorded.** List what this session was expected to handle and could
not finish in this turn - a blocked ask, a deferred fix, a call that was refused, a question left
open - with the reason it stopped and where its context lives. The next session starts exactly
where this one stopped; anything left out is lost when the turn ends.

**Exit through the tool.** Forge takes the note as you wrote it - there are no headings to fill
in, and your Step 2 document is the note. Write it where Step 2 says, then pass the same text to
`agents__handoff` as `brief` as the turn's last act. Memory first, then close out what is
running, then the note. Do not call `agents__handoff` for any other reason - a handoff starts at
`/handoff` or at the automatic trigger (fill or compaction).
