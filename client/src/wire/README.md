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

Taken from `busytools/forge` main at
`73b8751a45722bb22c8f79d2fef893b0810ef026`, where the socket landed, from
`crates/forge-server/tests/wire_fixtures/`:

- `home.json` is blob `c79eaedb28fbaf419e37814dcd9e950fe69331c2`
- `session.json` is blob `9846d24ab9e628e8437ba8f251d5814fc1a4d532`
- `usage.json` is blob `5b068cbab71cf6bf03767fecc3a4415d40fedeef`

The copies are byte-identical to those blobs and `salvage.test.ts` fails if
one drifts, which is what a formatter run did once and would again: `src/dev/fixtures/*.json`
is in `.prettierignore` for that reason.

## What moved since the first copy

The first copy was taken while the server was still building the socket, and
three things changed before it landed. Each is now drawn:

- **A project row is nested.** `projects[]` entries are
  `{project, work, tasks, crons, would_bind, chip}` rather than a bare
  project, so the home's `where` cell, its `what` cell, the refusal line and
  the row's chip are all reachable. The per-row working tree is the one that
  mattered most: it was a per-row read the snapshot did not carry, so the
  branch and the changed count could not be drawn at all.
- **`unseen` is a new top-level key** - the seats whose last turn finished
  while no client was showing them. It is the fact a late subscriber cannot
  reconstruct from the records, which is why it is carried rather than
  derived.
- **`forge_version` and `forge_version_short` are new**, and the header
  draws them rather than its own package version: the header states which
  forge is RUNNING, and the client is a different program.
- Also: `agents[]` gained `peer` and `peer_failure_at`, `dictate` gained
  `enabled` and `device`, and `usage.json` is a third subject that no page
  in this slice draws.

`tasks` and `crons` are empty in the fixture, so their element shapes come
from `crates/forge-server/src/transport/wire.rs` rather than from the file.
The fixture pins what it carries; the source pins the rest.
