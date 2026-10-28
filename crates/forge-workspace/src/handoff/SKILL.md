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

## Step 2 - Write the handoff

**Location: the session's project memory directory** - the one whose `MEMORY.md` auto-loads for
this project. The file is `handoff.md` in that directory; rewrite it in place each time. Never
write it to /tmp (invisible to a fresh session and wiped on reboot) and never into the
workspace's tracked tree (it is a generated doc; committing it pollutes git - the memory dir is
the gitignored home for exactly this).

**The document has these parts, in order:**

1. **State snapshot** - what is done and verified, what is in flight, what is clean/unchanged.
   Any work claimed as complete must have been verified (tests run, /health checked, output
   read) before the handoff says so.
2. **Next steps / open threads** - each with a pointer to where its context lives (memory file,
   commit, PR, plan path). The handoff references, it does not duplicate: do not restate content
   already captured in memory files, specs, plans, ADRs, issues, commits or diffs - name them by
   path or URL.
3. **Process facts** - anything the next session must know to work the way this one did
   (authorizations, review-loop requirements, watch/verify patterns), only what is not already
   recorded in memory.
4. **Suggested skills** - which skills the next agent should call the Skill tool for, by name.

Redact sensitive information (API keys, passwords, PII).

If the user passed arguments, treat them as the next session's focus and tailor parts 2 and 4
accordingly.

## Step 3 - Index

Add or update the `handoff.md` line in the project memory's `MEMORY.md` in the same pass. The
line leads with a status token (SHIPPED / IN FLIGHT / BLOCKED) plus a one-line hook - the index
line is what the next session actually sees; the file is only reachable through it.

## Forge: close out and exit

When forge hands the seat over, two things happen on top of the steps above.

**Close out what is running.** Anything this session started and did not finish must not outlive
it: a live sub-agent, a background command, a monitor, a process it spawned. For each one,
record in memory what it is and how to resume it - the command, the id it ran under, where its
output goes - then stop it. A successor that finds half-finished work and no record of the
process that owns it cannot tell the two apart.

**Leave nothing unfinished unrecorded.** List what this session was expected to handle and could
not finish in this turn - a blocked ask, a deferred fix, a call that was refused, a question left
open - with the reason it stopped and where its context lives. The next session starts exactly
where this one stopped; anything left out is lost when the turn ends.

**Exit through the tool.** Forge asks for the handoff under five headings - Now, Tried and
failed, Do not, Next, Done means - and your Step 2 document is that same note, so its parts land
as: the state snapshot in Now, the next steps and open threads in Next, the process facts in
Do not or Next, and the suggested skills in Next. Give the note the five headings, with the last
line under Now naming what you recorded in memory and where. Write it where Step 2 says and pass
the same text to `agents__handoff` as `brief` as the turn's last act. Do not call
`agents__handoff` for any other reason - a handoff starts at `/handoff` or at the fill threshold.
