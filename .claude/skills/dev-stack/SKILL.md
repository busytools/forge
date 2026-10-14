---
name: dev-stack
description: Invoke for feature-scale web work - client, server, or both - when the result needs the maintainer's eyes before merge: it stands up the depth the change needs - a per-feature scratch forge under /tmp with its own config, ports and store plus a dev client pointed at it, or, for a pure client change, the dev client alone - so the feature is built and verified live without touching the maintainer's config. Not for one-line fixes; those take the ordinary PR loop.
---

# dev-stack

One feature, one self-contained setup: a scratch `forge` under `/tmp` with a
dev client pointed at it (the deep depth), or the dev client alone against
the live forge (the shallow one), the maintainer's verification, the usual
PR + review loop, one post-review re-look for feature-scale work, merge,
teardown. The
live forge, the live client and `~/.claude` are never mutated or restarted,
at any depth.

**When it fires, and whose job that is.** Any feature-scale web change -
client, server, or both - that needs the maintainer's eyes before merge runs
this loop; one-line fixes take the ordinary PR loop instead. It runs at one of
two depths, set by what the change touches: a pure client change takes the
shallow path (recipe step 5) - the dev client alone, no scratch forge - while
anything touching server behaviour takes the full stack. Firing it is the
LEAD's act at plan time, not the worker's discovery: the lead reads this file
and threads its loop into the worker's charter, so the charter already carries
the depth, the verification steps and the teardown.

**One landing, never a piece.** A work cut - one feature, one issue, one
fix - lands whole: one branch, one review, one look, one merge. Never slice
it into pieces that land one after another, and never show or merge an
intermediate step on the way. The look is the one that cut is owed: a
feature waits for the user's own look before merge; a fix merges after the
review loop as usual. A feature is not split to land faster - the stack
exists so it can land whole and be looked at whole, exercised end to end
before it lands. Anything deliberately left out, or lost on the way, is
surfaced to the maintainer BEFORE merge rather than as a follow-up, and for
feature-scale work the post-review re-look (step 6) is mandatory, not a
courtesy.

**The first users refine this.** The recipe is measured, not sacred - when a
step costs time, breaks, or reads wrong in real use, say so to the lead; the
lead lands the correction as a small PR against this file. This file's
history is the record.

## The loop

1. **Plan the feature and pick its label** (the label names the worker, its
   task row and the stack directory).
2. **Spawn the worker with worktree isolation** (the recipe builds and boots
   from that worktree, never the main checkout). Its charter is this skill's
   loop; kick it. **The
   worker runs the model this session is running** - state which, and hold the
   worker to it (until #1825 lands the enforced `model` field on
   `agents__spawn`, verify by observation and say so). Interactive: true - the
   maintainer's verification is the worker's own question.
3. **The worker brings the stack up** (recipe below) - the deep depth; a
   shallow run has no stack and lives entirely in the client dev server
   (recipe step 7). Either way: client hot reload throughout, and server
   changes live from the branch - there is no release and no restart of
   anything of the maintainer's.
4. **The maintainer looks**, via the worker's AskUserQuestion, one page or one
   behavior at a time, against the stack (deep) or the dev client (shallow).
5. **PR + the lead's review loop.** Fixes land on the branch; the stack
   rebuilds and restarts from it.
6. **For feature-scale work, the post-review re-look** (Ved's ruling): after
   the loop's fixes, the maintainer looks once more before merge, so a fix
   round cannot change what was already approved. Small one-liners merge after
   the loop as usual. **If the re-look finds anything**: visual-only → a small
   commit and CI, then merge; substantive → back through step 5, where the
   three-round cap applies as everywhere else. The stack is rebuilt from the
   branch before every look, so what he sees is always the head.
7. **Merge, then teardown** (invariants below). One unit, nothing left behind.

## The recipe (measured end to end, 2026-10-06)

`STACK=/tmp/dev-stack-<feature>`

1. **The tree and config**
   - `mkdir -p $STACK/config/forge $STACK/home`
   - Write `$STACK/config/forge/forge.toml`:
     - `[[accounts]]` - ONE account, its flat `token` key copied read-only
       from the main config (`~/.claude/forge/forge.toml`). **A non-Anthropic
       one where the config carries one** (an `openrouter` provider block) -
       a scratch must never spend the Anthropic pool; name which account in
       the worker's report.
     - `[[orgs]]` + `[[orgs.projects]]` - the repo's path, `auto_start = true`,
       a `model`, `permission_mode = "auto"`.
     - `[server] enabled = true, bind = "127.0.0.1", port = 8791` and
       `[gateway] port = 8788` - a distinct port pair per stack (8791+idx /
       8788+idx for parallel stacks).
     - **A dictation-touching stack points `models_dir` at the REAL cache,
       spelled ABSOLUTELY** (the live config's value with `~` expanded to the
       real home at setup time): a `~` in the scratch config re-expands under
       the stack's HOME, and the default is the platform cache under that
       same HOME - so anything but the absolute path silently fetches and
       loads its own ~3 GB of models.
2. **The binary**
   - **Always build from the tree** (the worker's worktree) -
     `nice -n 10 cargo build -p forge-server --bin forge -p forge-server --bin forge-protocol-client`
     - warm-cache it is about a minute, and the branch's server is what runs,
     which is the point. **Never the installed binary**: its parser denies
     unknown fields, so a
     recipe-shaped scratch config fails the parse (`unknown field 'server'`)
     and the process exits without binding anything.
3. **Boot headless** - the `forge` binary is a headless server, so no pty is
   involved; `script(1)` keeps the launch shaped as it always was (tmux is not
   installed on this machine):
   - Write `$STACK/boot.sh`: `export HOME=$STACK/home`, `export
     CLAUDE_CONFIG_DIR=$STACK/config`, then `exec <forge binary> --log-file
     $STACK/forge.log`.
   - `script -q /dev/null /bin/sh $STACK/boot.sh < /dev/null &`
   - **Launch it `nohup`-detached, never bare `&`**: a background job started
     inside a tool call can die with that call's process group (measured: a
     scratch was gone ~30s after launch while another stack took its ports,
     and a bare launch has also held for hours; detaching costs nothing).
     Wrap this line as `nohup script -q ... > $STACK/boot.out 2>&1 &` from a
     `launch.sh`.
   - **The HOME redirect is mandatory, not optional**: the durable store
     resolves under `HOME`, so without it the scratch forge points at the
     real `db.redb`, fails redb's exclusive lock, and boots degraded with no
     persistence (#1826 is the forge-side fix; until it lands, this is the
     skill's rule).
4. **Verify the boot**: `lsof -nP -iTCP:8791 -sTCP:LISTEN` and `8788` both
   hold the forge pid; `$STACK/home/Library/Application Support/forge/`
   carries a fresh `db.redb`; the log has no store-failure line; the first
   session connects ~3s in (the boot spawn is held until the account's usage
   probe settles - that is normal). **Confirm it is YOUR stack**: the build
   stamp on the home (it rides the greeting too) is the invariant check, and
   the roster's org is the cross-check - together they catch a wrong-forge
   connect in one read.
5. **The client - depth follows the change (Ved's ruling, 2026-10-10).** A
   pure client change runs SHALLOW: the dev client from the worktree on its
   own (`npm --prefix client run dev`, step 7), pointed at the live forge's
   own address - no scratch forge, no `/tmp` tree, the change in front of the
   maintainer as fast as vite starts. The cost, named: that tab is another
   answering client on the live forge, so a turn can park on it. Anything
   that touches server behaviour runs DEEP - the full stack - with the client
   pointed at the stack:
   - Browser tab (the deep default): the dev client from the worktree - hot
     reload, the maintainer's own browser. Point it at the stack through its
     own connect screen: type `127.0.0.1:8791` (a bare `host:port`; the field opens on the
     live forge's address, so the scratch one is a deliberate edit). The client
     keeps the last address that ANSWERED (`localStorage forge.address`,
     written only on a successful connect) and attempts it next open: stack
     up, straight to the home; stack dead, the door opens with the address
     preset and the failure drawn; a dead address reports in about 5 seconds
     (the greeting deadline), not a hang. Point it back at `127.0.0.1:8790`
     when the stack dies.
   - App shell (when the feature needs one - browser hosting, dictation
     capture): the Tauri debug shell from the worktree, same connect screen,
     same address. **A scratch shell is a rebuilt BUNDLE with its own
     identifier** (the browser E2E rebuilt under `dev.vedhavyas.forge.e2ebrowser`)
     - and it must be the built bundle, not `tauri dev`: an unbundled dev
     shell's storage is keyed by process name rather than by identifier, so
     every client on this Mac shares it, a scratch window auto-connects to
     the LIVE forge, and no in-app door leads back to the connect screen.
     (The built bundle is also the client that does not collide when another
     stack already holds the ports.)
     **Point a scratch shell at the stack with a `--config` override, not by
     typing in its connect screen**: the window url may carry the address,
     and the shell dials it at launch (`serverParameter` outranks the
     remembered one), which is one less thing for the maintainer to do and
     lands the look straight in a session -
     `{"identifier":"dev.vedhavyas.forge.<name>","bundle":{"createUpdaterArtifacts":false},"app":{"windows":[{"title":"forge <name>","width":1440,"height":900,"url":"/?server=127.0.0.1:<port>"}]}}`
     written to a file and passed as `--config <path>`. Two details that cost
     a look: **the url is `/?server=...`, never `index.html?server=...`** -
     the router parses the pathname, and the document's own name is not a
     route (it landed the window on the not-a-page screen) - and
     `createUpdaterArtifacts: false` keeps the build off the updater signing
     key. A fresh identifier also holds no microphone grant, so a
     capture feature's first take raises its own TCC prompt; say so before
     the look rather than letting him read it as a bug.
   - The stack takes browser WebSockets as they come - a raw handshake with an
     Origin header answers 101 plus the greeting; no CORS step exists.
6. **Drive headlessly when useful**: `forge-protocol-client --url
   ws://127.0.0.1:8791/socket --script <file> --seconds N` (its script verbs:
   subscribe, unsubscribe, more, devices, command, ask - a prompt rides
   `command` or `ask`) - the harness's own client.
7. **The client's own bring-up** (a fresh worktree): `npm --prefix client ci`
   once (~2.5 min niced), then `npm --prefix client run dev` - vite on
   `http://localhost:1420` (port fixed, no browser opened by itself). **Reach
   it at `localhost:1420`, never `127.0.0.1:1420`** - vite binds IPv6 loopback
   only and the IPv4 address refuses. `just client-dev` is the same server
   under the Tauri window (needs a display): use it when a person is looking.
8. **Seed the scratch with whatever the feature needs to be exercised** - a
   test subscription, a task, a cron - through the scratch's own sessions or
   tools (a scratch session carries the `mcp__forge__*` family; it lands in
   the scratch store only and dies with it). Two cautions: the CLI's auto-mode
   classifier can refuse a seed phrased suspiciously (a prompt carrying a
   marker plus "delete me" was refused twice; plain wording passed - n=1 per
   wording, so rephrase plainly and retry, it is not a forge bug), and the
   socket carries no cron/task verbs - the session's own list tools are the
   read-back path.
9. **Teardown** - `kill -TERM "$(lsof -t -iTCP:8791 -sTCP:LISTEN)"` (by port,
   never a pattern), confirm the port and pid are gone, then
   `find $STACK -depth -delete` (rm -rf is permission-blocked on this
   machine; find -delete passes). The lock sits inside `$STACK` under the
   HOME redirect, so the tree delete covers it. **The stack's windows and
   browser processes are its own to reap**: whoever brought the stack up
   closes its app windows and kills its own browser processes by port or
   PID - nothing of the stack outlives it, and another worker's windows are
   theirs, never yours.
10. **Teardown invariants (prove, don't assume)**: the live forge's PID and
   start time unchanged; the real store's holder unchanged; `~/.claude` read,
   never written (checksum the config if anything is in doubt).

## Gotchas that cost time (ranked)

1. **A restarted worker's worktree can be at the LIVE CHECKOUT'S STALE HEAD**
   (measured: a restart re-provisioned the spike's worktree at the release
   era, pre-`[server]`, and the stack then refused its own config). Before
   booting, `git rev-parse HEAD` against the branch's base; if it lags,
   fast-forward the worktree to origin/main (non-destructive).
2. **The build stamp goes stale in a linked worktree** (measured): `build.rs`
   watches `../../.git/HEAD` and friends, but a worktree's `.git` is a FILE,
   so the watches never arm and the binary can report the wrong commit while
   running another. Until the forge-side fix (#1829) lands, `touch`
   `crates/forge-server/build.rs` (mtime only - the Rust workspace's one
   build script; a bare `touch` on any other `build.rs` path creates an
   empty file and the build dies on it) before a rebuild in a worktree, and
   read the boot log's stamp with that in mind.
3. The store follows HOME, not the config dir - see the mandatory redirect
   above and #1826.
4. Both default ports are the live forge's (8790/8787). A taken gateway port
   holds the spawn gate shut and is drawn with the port named; **a taken
   SOCKET port fails invisibly**: the log carries
   `server_socket_bind_failed` / "the socket is not serving" while the ports
   read bound (the other stack's listeners) and the handshake answers you
   with ANOTHER stack's roster. **lsof the pair before booting, and again
   immediately before driving it** - another stack can take the pair in
   between (measured 2026-10-12: a scratch lost both ports that way and
   moved to 8792/8789). Take the next one; a bind-failure line is a
   collision to fix the pair for, not to investigate. One pair per stack.
5. `[web]` is refused by name; scratch configs use `[server]` + `[client]`.
6. A fresh config dir has no trust record; the CLI then ignores the project's
   permission-allow entries and says so with the exact remedy
   (`hasTrustDialogAccepted` in `.claude.json`, or accept the dialog once).
   Sessions still run under `permission_mode = "auto"`.
7. The boot spawn rides the server's own boot wave (the `<PROJECT>`
   positional starts that project's sessions at boot), so no pty or view is
   involved anywhere. `script -q /dev/null <cmd> < /dev/null` is kept in the
   launch shape only for uniformity.
8. Pass `--log-file` into the stack - otherwise the scratch forge appends to
   the shared app-support log beside the live one.
9. The lock is `locks/<16-hex hash of the canonicalized config dir>.lock`,
   held for process lifetime, no stale cleanup; never delete it while a forge
   holds it. A second boot on the same config dir refuses by name with the
   holder's PID - that guard is correct and expected.
10. Ports, paths and teardown all use the *canonicalized* path (`/tmp` is
    `/private/tmp`), so one stack keeps one hash.
11. **A wiped store is not a clean reset. The session transcripts live under
    `$STACK/config/projects/<slug>/` (`<slug>` is the project path's
    canonicalized slashes as dashes: `/tmp/.../proj` is
    `-private-tmp-...-proj`), the boot's catalog scan adopts the last one, and
    the "fresh" run RESUMES that session** (measured 2026-10-12: an e2e phase
    went green off the PREVIOUS run's parked ask while its own prompt queued
    behind the stale turn - one run's whole timeline off by one). The clean
    reset is four steps, not one: kill by port; `find
    $STACK/config/projects/<slug> -depth -delete`; `rm -f` the redb under
    `$STACK/home/Library/Application Support/forge/`; restore the seed.
12. **A text probe reads the agent's own prose** (measured 2026-10-12, twice
    in one run). `document.body.innerText.includes('<the card's words>')` can
    match the SESSION'S OWN CHAT - an agent narrating what it just exercised
    put the card's exact words on the page while the dock drew nothing, and
    the probe read green over a click that then found no button. Ask the DOM
    for the CONTROL, never the words: the dock's own button, the element's
    presence, the frame's payload. (The harness's older cousin: matching a
    tool's NAME in a frame proves the call was sent, not that it succeeded.)

## Could not prove (watch these)

- The shallow depth end to end (the dev client alone, pointed at the live
  forge).
- A taken scratch gateway port's refusal (read from code, not exercised).
- Interactive trust acceptance; a release/perf build; two stacks at once; any
  keychain interaction from the redirected HOME's child sessions.
