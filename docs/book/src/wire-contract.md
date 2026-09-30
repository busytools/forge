# The wire contract

The `claude` CLI is the source of truth. forge spawns it and speaks
stream-json to it over stdio. forge never re-implements the agent loop
and never calls the Anthropic API directly.

That leaves exactly one hard compatibility requirement: what forge
writes to the CLI's stdin has to be what `claude` expects, and what
forge decodes from its stdout has to cover what `claude` actually
emits. A difference either way is a bug in forge, not a feature gap.

There is no compatibility requirement in the other direction. forge-sdk
is not a port of the Python `claude-agent-sdk` and carries no
public-API parity contract with it; the two are peer clients of the
same binary.

## The conformance harness

`forge-test-harness` is how the wire requirement is enforced rather
than asserted. Each scenario runs in one of two modes.

**Replay mode** is the default and runs on every `just check`. It loads
a committed baseline trace and feeds every inbound line through
forge-sdk's `decode_dispatch`. It costs nothing and hits no API. The
test fails if any line produces a decode error, decodes to an unknown
message type, or decodes to an unknown control-request subtype.

**Live-capture mode** runs only when you ask for it, with
`FORGE_WIRE_CAPTURE=1` and `--run-ignored only`. It spawns the real
`claude`, drives forge-sdk through the scenario, and writes the full
stdin and stdout trace to `target/wire-traces/`. It burns real API
tokens, so it is not part of any automated run.

```bash
just conformance                                  # replay everything, offline
just conformance-capture-sdk wire_capture_trivial_prompt   # one live capture
```

The argument is a nextest test name, not a baseline name, and the two
namespaces do not always match. It is matched as a substring, so a
loose argument selects several live captures and bills for all of them.
An empty argument is rejected outright rather than passed through,
because with no filter left it would capture everything against the
real API.

## Baselines

Committed baselines live at:

```
crates/forge-test-harness/baselines/sdk/<PINNED_CLI_VERSION>/<scenario>.jsonl
```

`PINNED_CLI_VERSION` is a constant in the harness. The directory name
is that constant, so bumping the pin means re-capturing the baselines
under a new directory. Between the bump and the re-capture, replay is
expected to fail.

Each line is a JSON object recording a direction and a raw wire line.
Traces are redacted on the way out, at serialisation, so both the
live-capture write and baseline promotion go through the same
redaction point.

### Baselines have to be live-captured

A baseline is a recording of the bytes that actually crossed the pipe.
It is not derived from anything else and cannot be hand-written from a
schema, because the point of the artifact is to be evidence of what the
CLI really sent, not of what we believe it sends. A synthesised
baseline would replay cleanly against a decoder that is wrong in the
same way the synthesis was.

The CLI's own on-disk session files are not a substitute either. They
are a different format from the wire: the harness has to run them
through a transformer to get them into wire shape before the decoder
will accept them. There is a separate opt-in probe that does exactly
that, pointed at a directory of real sessions with
`FORGE_REAL_SESSIONS`, so decoder regressions can be caught against
accumulated real-world data without committing any of it.

### What replay does and does not check

Replay reads inbound lines. It proves forge can decode everything the
CLI sent, which is the half that regresses silently. It does not
generally assert the bytes forge puts on the wire; the one outbound
check is on the in-process MCP `initialize` handshake, where the test
re-runs the current server code against the recorded request and
compares that live answer to the recorded response, rather than just
checking the two recorded lines agree with each other.

## Shipping new wire surface

If your change adds a control-request subtype, a message type, a hook
event or a tool integration, it needs three things in the same pull
request:

1. A live-capture scenario that exercises it.
2. The captured baseline committed under the pinned CLI version's
   directory.
3. A clean replay: every inbound line round-trips through the decoder
   with no unknown variants and no decode errors.

`.claude/skills/claude-cli-upgrade/` in the repository holds the CLI
version-bump ritual: the capture command, the baseline layout, and how
to add a scenario.

## The socket's own contract

The CLI wire is not the only one forge records. A client reads a second
wire, the socket `forge-server` serves, and it has the same failure mode
one layer up: the server renames a field, every test passes, and the page
draws blank because nothing compares the two ends.

Those shapes are recorded under:

```
crates/forge-test-harness/baselines/socket/<PROTOCOL_VERSION>/{frames,chat}.json
```

They live under `baselines/` beside the CLI captures and are not captures
themselves: nothing here is recorded live. Both are derived from the
current code, which is why regenerating one is a deliberate act.

`frames.json` carries every variant of the five enums whose tags the socket
itself chooses - `ServerMessage`, `SessionUpdate`, `Subject`, `Command` and
`ClientMessage` - with the wire name each encodes as. The census behind it
is one list per enum expanded into both a `match` with no wildcard arm and
the names the record is built from, so a variant the server starts or stops
sending is a compile error before it is ever a failing test, and there is
no second list to drift. The client writes a `ClientMessage` tag by hand,
so a rename there is a message the server refuses rather than a page that
draws blank.

A `Message` also crosses, inside `chat_appended`, and its variant tags are
not pinned here: they are the CLI's words rather than forge's, and what
`chat.json` records of a message is the keys of the fields its variants
carry.

`chat.json` carries the key set of the `Message` payload the chat fold
reads, walked off the committed SDK baselines rather than derived from a
schema. That is the payload `chat_appended` carries, and it is where a
renamed field stops matching a name a page reads. `frames.json` also
carries the path-and-key shape of two sampled frames - the `chat_appended`
update and a `page` with one turn in it, so the paging fields a client
reads are pinned rather than declared.

**Keys and paths only, never a value.** A key that is not an identifier is
read as a map key and collapsed to its value's shape, because a model name
or a question's own text would otherwise move the record whenever the
content moved.

Both records are rebuilt from the current code on every `just check` and
compared with the committed copy. Regenerate one deliberately:

```bash
just conformance-record-socket
```

Read the diff against the client before committing it. A field that moved
is a page that draws blank, so the question the diff answers is which
client read follows it. **Nothing reads the client for you**: the records
say what the server emits, and whether a page reads those names is a person
comparing the two against `client/src/`. The check is on the record, not on
the agreement, and a change that renames a field without touching the page
is caught here while a page that reads the wrong name is not.

**Some limits print themselves on every run.** The three subject fixtures
report what they actually pin: how many keys they carry, how many are
`null` (the key exists, nothing behind it is pinned), and how many
collections they carry empty, where an element shape is pinned nowhere at
all and a field renamed inside one is invisible to every pin in the tree.

**Others are declared rather than printed.** A payload is pinned only for
the two frames sampled; the fields inside a variant nobody samples are not
pinned at all. A container's `rename_all` and any variant-level
`#[serde(rename)]` are covered for every variant, because the names are
read out of serde rather than derived from the Rust ones - but that reads
serde's DESERIALIZE side, and a client reads what serde WRITES. A split
renaming, which names a variant one way out and another way in, is
therefore invisible to the names here; the test asserts by source that no
such renaming exists in the three files carrying these enums.

That last check depends on serde's own unknown-variant message, which it
formats rather than contracts: the parse finds nothing if the wording
changes, and the emptiness assertion fails loudly rather than the
comparison passing against an empty set.
