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
