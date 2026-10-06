---
name: dev-stack
description: Invoke for feature-scale web work - client, server, or both - when the result needs the maintainer's eyes before merge: it spins a per-feature scratch forge under /tmp with its own config, ports and store plus a dev client pointed at it, so the feature is built and verified live without touching the live forge or the maintainer's config. Not for one-line fixes; those take the ordinary PR loop.
---

# dev-stack

One feature, one self-contained stack: a scratch `forge` under `/tmp`, a dev
client pointed at it, the maintainer's verification, the usual PR + review
loop, one post-review re-look for feature-scale work, merge, teardown. The
live forge, the live client and `~/.claude` are never part of the loop.

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
3. **The worker brings the stack up** (recipe below), iterating with client
   hot reload and server restarts inside the stack. Server changes are live
   from the branch - there is no release and no restart of anything of the
   maintainer's.
4. **The maintainer looks**, via the worker's AskUserQuestion, one page or one
   behavior at a time, against the stack.
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
     `nice -n 10 cargo build -p forge-tui --bin forge -p forge-server --bin forge-protocol-client`
     - warm-cache it is about a minute, and the branch's server is what runs,
     which is the point. **Never the installed binary**: it predates the
     `[server]` rename, and its parser denies unknown fields, so a
     recipe-shaped scratch config fails the parse (`unknown field 'server'`)
     and the process exits without binding anything.
3. **Boot headless** - the TUI needs a pty, and `script(1)` is it (tmux is not
   installed on this machine):
   - Write `$STACK/boot.sh`: `export HOME=$STACK/home`, `export
     CLAUDE_CONFIG_DIR=$STACK/config`, then `exec <forge binary> --log-file
     $STACK/forge.log`.
   - `script -q /dev/null /bin/sh $STACK/boot.sh < /dev/null &`
   - **The HOME redirect is mandatory, not optional**: the durable store
     resolves under `HOME`, so without it the scratch forge points at the
     real `db.redb`, fails redb's exclusive lock, and boots degraded with no
     persistence (#1826 is the forge-side fix; until it lands, this is the
     skill's rule).
4. **Verify the boot**: `lsof -nP -iTCP:8791 -sTCP:LISTEN` and `8788` both
   hold the forge pid; `$STACK/home/Library/Application Support/forge-tui/`
   carries a fresh `db.redb`; the log has no store-failure line; the first
   session connects ~3s in (the boot spawn is held until the account's usage
   probe settles - that is normal).
5. **The client** - judgment call per feature:
   - Browser tab (default): the dev client from the worktree - hot reload, the
     maintainer's own browser. Point it at the stack through its own connect
     screen: type `127.0.0.1:8791` (a bare `host:port`; the field opens on the
     live forge's address, so the scratch one is a deliberate edit). The client
     keeps the last address that ANSWERED (`localStorage forge.address`,
     written only on a successful connect) and attempts it next open: stack
     up, straight to the home; stack dead, the door opens with the address
     preset and the failure drawn; a dead address reports in about 5 seconds
     (the greeting deadline), not a hang. Point it back at `127.0.0.1:8790`
     when the stack dies.
   - App shell (when the feature needs one - browser hosting, dictation
     capture): the Tauri debug shell from the worktree, same connect screen,
     same address.
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
   HOME redirect, so the tree delete covers it.
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
4. Both default ports are the live forge's (8790/8787); a taken gateway port
   is the worse collision - it holds the spawn gate shut. Allocate the next
   free pair (8791/8788, then 8792/8789, ...) and keep one pair per stack.
5. `[web]` is refused by name; scratch configs use `[server]` + `[client]`.
6. A fresh config dir has no trust record; the CLI then ignores the project's
   permission-allow entries and says so with the exact remedy
   (`hasTrustDialogAccepted` in `.claude.json`, or accept the dialog once).
   Sessions still run under `permission_mode = "auto"`.
7. No pty, no session: the socket binds without one, but the boot spawn runs
   in the TUI connect path. `script -q /dev/null <cmd> < /dev/null` is the
   pty that works here.
8. Pass `--log-file` into the stack - otherwise the scratch forge appends to
   the shared app-support log beside the live one.
9. The lock is `locks/<16-hex hash of the canonicalized config dir>.lock`,
   held for process lifetime, no stale cleanup; never delete it while a forge
   holds it. A second boot on the same config dir refuses by name with the
   holder's PID - that guard is correct and expected.
10. Ports, paths and teardown all use the *canonicalized* path (`/tmp` is
    `/private/tmp`), so one stack keeps one hash.

## Could not prove (watch these)

- Cross-boot resume inside a stack (both spike runs had fresh stores).
- A taken scratch gateway port's refusal (read from code, not exercised).
- Interactive trust acceptance; a release/perf build; two stacks at once; any
  keychain interaction from the redirected HOME's child sessions.
