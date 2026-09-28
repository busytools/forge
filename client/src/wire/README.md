# The wire fixtures

The server's own test fixtures, copied so the pages type their props against
the shapes the server actually sends.

**They live at `client/src/dev/fixtures/`, not beside this file.** The
directory is `dev/` because the shipped app carries no fixture: the bytes are
read by `dev/fixture.ts` behind a DEV guard and by `dev/fixture.data.ts` for
tests, and `dev/fixture.test.ts` builds the app and fails if they reach the
bundle. What is here is the record, the types they are read through, and the
narrowing at the boundary.

## Where the copies came from

Taken from `busytools/forge`, branch `forge-server-socket`, at commit
`cf1b935f16fbd8dc4703001708666fd95e1daadf`:

```sh
git show origin/forge-server-socket:crates/forge-server/tests/wire_fixtures/home.json
git show origin/forge-server-socket:crates/forge-server/tests/wire_fixtures/session.json
```

`home.json` is blob `3913de88`, which that branch still carried unchanged
at `3f0d755d`. It stays that way here: `crates/forge-server/tests/transport.rs`
pins both shapes with a test, so a fixture cannot drift on the server side,
and a hand-edited copy here would be the drift this pair exists to prevent.

## The branch has moved, and the copies are stale on purpose

**Do not re-sync them yet. This is the reconciliation Task 2 starts with.**

As of `a2cadb2b842cce588c8d75f55f7e6339de3542a8` the server's `home.json` is
blob `bbc3f97c`, first reshaped at `5b3b2667` and unchanged since. **All four
reads this slice reported missing have arrived**, and `dictate.enabled` came
with them:

- **Each entry of `projects` is now `{project, work, tasks, would_bind}`**
  rather than a bare project. So `HomeWire.projects` no longer describes
  what a live socket sends, and a real snapshot would render rows whose
  `org`, `name` and `key` sit one level down under `project`.
  `work` carries `{branch, changed, gate}`, which is the per-row working
  tree `.row .where` had none of. `tasks` is what `.row .what` and its
  status chip needed. `would_bind` is the missing half of the refusal line.
- **`forge_version` and `forge_version_short` are new top-level keys**, which
  is the header version nothing carried.
- **`dictate` gains `enabled`**, beside `snapshot` and `models_dir`.

`session.json` is blob `ade18792` and has not moved.

So `homeFrom`'s types, `view.ts`'s gather and the README's table of absent
reads all describe the pre-`5b3b2667` shape. Re-syncing is one commit and
one pass through those three; it waits until the server's Task 10 settles,
because it may move again.

## A note for whoever wires the first icon

The sprite is ported and tested as DATA: the test reads the mockups for the
ids they use and asserts the sprite carries them. Nothing tests it as a
DRAWN page, so removing `<Sprite />` from `Shell.svelte` is green today.
Live but unchecked, because no page here draws an icon yet. Task 5's
inspector is the first that will.
