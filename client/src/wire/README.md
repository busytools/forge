# The wire fixtures

The server's own test fixtures, copied here so the pages type their props
against the shapes the server actually sends.

Taken from `busytools/forge`, branch `forge-server-socket`, commit
`cf1b935f16fbd8dc4703001708666fd95e1daadf`:

```sh
git show origin/forge-server-socket:crates/forge-server/tests/wire_fixtures/home.json
git show origin/forge-server-socket:crates/forge-server/tests/wire_fixtures/session.json
```

The copies are byte-identical to that branch's blobs. They stay that way:
`crates/forge-server/tests/transport.rs` pins both shapes with a test, so a
fixture cannot drift there, and a hand-edited copy here would be the drift
this pair exists to prevent.

When that branch merges, these two files are the same artifact as the ones
under `crates/forge-server/tests/wire_fixtures/`, and the copy is deleted.
