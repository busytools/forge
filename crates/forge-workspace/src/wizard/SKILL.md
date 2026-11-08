---
name: wizard
description: Generate an interactive bash wizard that walks a person through steps only they can perform - provisioning infrastructure, setting up credentials or CI secrets, walking an unfamiliar third-party dashboard, or running a one-off migration or cutover - driven through this seat's terminal, opening dashboards in the forge browser, finding a credential in settings or, when the session carries that family, the vault before asking, and recording the stage so a handoff can resume it. Don't invoke this for steps the agent can perform itself.
disable-model-invocation: false
---

# Wizard

A **wizard** is a bash script that walks a person, step by step, through a manual procedure
that's tedious to do by hand and tedious to re-explain to an agent every time. It opens each
destination, says exactly what to do, captures the values, writes them where they belong
(`.env`, `gh secret`, `gh variable`), confirms before anything irreversible, and shows how many
stages are left. It might configure third-party services, run a one-off migration, or move the
project from one state to another.

The UX is already solved by [template.sh](template.sh): stage-by-stage progress, confirmation
gates, hidden secret entry, idempotent `.env` upserts, `gh secret`/`gh variable` writes, and a
closing summary. **Your job is only to scope the procedure and author its stages.** The library
above the `STAGES` marker is identical in every wizard; that consistency is the point, so never
hand-edit it.

A wizard is ephemeral by default: built for one run, saved to a scratch or `scripts/` path, and
deleted when the job's done. Commit it only when the person wants a repeatable setup path that
should live in the repo.

## Who runs it, and where

**The agent runs the wizard in this seat's own terminal; the person only performs the stages
that need a human.** Do not hand the script over for them to run by themselves.

- `terminal__open` gives the seat a terminal: a real PTY on the person's own shell, in the
  session's cwd. Run the wizard there, not from a background command - the hidden entry and the
  confirmation gates need a terminal.
- `terminal__read` shows its screen and `terminal__write` types into it. Use them to follow the
  run and to answer anything the script asks that you already know.
- `terminal__hand_off` gives the keyboard to the person for the one thing only they can do - a
  password, a `sudo` confirmation, a sign-in, a value only they have. **You are blind while the
  card is parked**: `terminal__read` and `terminal__write` are refused until they answer. Read
  the outcome back with `terminal__read` afterwards and never assume what they typed.
- `terminal__close` when the wizard is done, so the shell it held goes away.

The tool descriptions carry the arguments and the exact behaviour; read them there rather than
from this page.

## Destinations: this seat's browser

When a stage sends the person to a dashboard, open it in the seat's browser and hand the tab
over. That is the plan: never send them to their own browser and never print a bare URL for
them to retype.

- `browser_navigate` opens the page in this session's own tab, in the profile forge resolves
  from the org and project.
- `browser_hand_off` raises that tab on their screen and waits. Use it when they must sign in
  or click something you cannot: they act on the very page you prepared. Their answer lowers
  it; it does not close the tab.

`open_url` in the script is the fallback for when no browser is available: it opens the URL in
whatever the machine has. It is the safety net, not the plan. Where you do not know the current
page or the exact control, say so and check the docs or ask - never invent UI steps that may
not exist.

## Credentials: settings, then the vault, then ask

Before a stage asks the person for a value, look for it in this order:

1. **`settings__get`** when the value is a key in `forge.toml`. `settings__index` lists what
   exists and `settings__get` answers the effective value; use it for anything forge itself is
   configured with.
2. **`vault__search`, then `vault__get`**, when it is a stored credential and your tool list
   carries the vault family. The search answers metadata only, so reveal the one field you need
   with `vault__get`. If the family was withheld from this session, ask your lead for it rather
   than working around it.
3. **`ask_secret` in the terminal** when it is in neither. Hidden entry, in the seat's terminal,
   written straight to where it belongs.

Never ask for a secret in chat, and never write one into a file that is not its destination: a
key goes to `.env`, to `gh secret`, to `gh variable`, or to the vault, and nowhere else. A value
found in settings or the vault is written to its real destination by its stage, never embedded
in the script.

## A wizard that outlives the session

A long procedure can outlive the turn that started it. Keep its state at `wizards/<slug>.md` in
the session's project memory directory, as a current-state document rewritten in place:

- the stage the wizard reached, and the stage it is on;
- every value captured, by name and by destination - never the secret itself;
- what remains, and what stopped a stage;
- how to re-run it: the script's path and the environment it needs.

**The seat decides where that file lives.** A lead keeps it at `wizards/<slug>.md`; a worker
keeps its stage in its own `workers/<label>.md` in the same directory, the way the always-on
rule has it, and tells its lead where it is. A handoff points at that file by path. The handoff
procedure itself is the `handoff` skill's, not this one's.

## Process

### 1. Scope the procedure

Work out every manual step the person must take and every value captured along the way. Read the
repo first, don't ask cold:

- For setup: `.env`, `.env.example`, `.env.*`, `README`, `docker-compose*`, framework config,
  and `.github/workflows/*` (every `secrets.*` / `vars.*` reference is a value the wizard must
  produce).
- For a migration or transition: the current state, the target state, and the irreversible
  actions between them.

Then show the person the ordered list of stages and the values each produces, and confirm: they
may add, drop, or reorder.

**Done when:** every stage is named in order, and for each captured value you know (a) where the
person gets it, (b) where it is written (`.env`, a GitHub secret, both, or nowhere - some stages
are pure actions), and (c) whether it is secret, for hidden entry, or public. A value that
already exists in settings or the vault says so here, so its stage offers it rather than asking.

### 2. Map each stage's journey

For each stage, write the precise path a person follows: which page to open, what to do there,
where a value is shown, and which variable it fills - for example "Dashboard -> Developers -> API
keys -> Reveal test key -> copy". Where you do not actually know the current UI or the exact
command, say so and ask the person or check the docs: never invent steps that may not exist.

**Done when:** every stage traces to concrete instructions a stranger could follow.

### 3. Author the wizard

Copy `template.sh` to the target path. Replace the example stage with one `stage` per step, in
dependency order. Use the library helpers: `stage`, `say`/`step`, `open_url`, `ask`/`ask_secret`,
`write_env`, `set_secret`/`set_var`, `pause`/`confirm`. Set `TOTAL_STAGES` to the number of
stages you wrote.

Hold the bar the template sets: open the destination before asking for its value, use
`ask_secret` for anything secret, `write_env` every persisted value, `set_secret` only the values
CI actually needs, and `confirm` before any irreversible action. Each `stage` clears the screen so
only the current step is visible: keep a stage to one focused task so nothing the person needs
scrolls away. Don't touch the library above the marker.

**A gate needs its own arm.** `confirm` returns non-zero when the answer is no, and a bare call
would end the whole wizard there under `set -e`. Give every gate both answers:

```bash
if confirm "Delete the old webhook?"; then
  delete_webhook
else
  note "kept the old webhook"
fi
```

**Done when:** the script holds one `stage` per step from step 1, `TOTAL_STAGES` matches, and the
library is byte-identical to `template.sh` above the marker.

### 4. Verify and run it

- `bash -n <script>`; run `shellcheck` if available.
- `chmod +x <script>`.
- Trace it statically first: every value from step 1 is captured and lands where step 1 said, and
  every `set_secret` name exactly matches a `secrets.*` reference in CI.
- Then run it in the seat's terminal and walk the stages with the person: open each destination
  in the browser, hand the tab over where they must act, and hand the terminal over where the
  input is theirs alone. Read the outcome back after every hand-off.

**Done when:** the wizard has run to its closing summary, or the stage it stopped at is recorded
in `wizards/<slug>.md` with what remains.

## Boundaries

- Never invent a UI step that may not exist. Say you do not know, then check the docs or ask.
- One focused task per stage: a stage asking for three unrelated things loses the first one off
  the top of the screen.
- A wizard the person asked to keep goes in the repo and gets linked from the README, so the next
  person runs the script instead of asking again.
