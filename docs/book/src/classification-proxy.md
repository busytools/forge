# The wire-classification proxy

forge runs a man-in-the-middle HTTPS proxy inside its own process and
routes the `claude` subprocess through it. This page describes what
that does, because it changes what happens on your machine and on your
network, and you should know about it before you run forge.

## What it does

At startup, before any session can spawn, `Workspace::new`:

1. Generates a self-signed certificate authority if one does not
   already exist on disk, and loads it.
2. Binds an HTTP proxy listener on a random port on `127.0.0.1`.
3. Builds the TLS context the proxy uses to reach upstream.
4. If it is chaining through an upstream proxy, builds that connector.

Steps 1, 2 and 4 can fail, and any of them stops forge starting: the CA
directory not being writable, key generation or self-signing failing,
the port not binding, or an unparseable upstream proxy URL. Step 3 has
no reachable failure path in practice, because a trust store it cannot
extend degrades to the built-in roots with a warning rather than an
error.

Where it does start, it starts fully: there is no degraded mode in
which sessions spawn without a proxy attached. The error reads:

```
wire-classification rewriter proxy failed to start: <reason>. forge
refuses to spawn sessions without a healthy proxy because the wire
shape Anthropic sees determines billing tier
```

The proxy starts unconditionally, even when every account in your
`forge.toml` has opted out of using it.

Each spawned `claude` subprocess then gets three environment
variables, unless the account it runs under sets `proxy = false`:

```
HTTPS_PROXY=http://127.0.0.1:<port>
HTTP_PROXY=http://127.0.0.1:<port>
NODE_EXTRA_CA_CERTS=<app-support>/ca/ca-cert.pem
```

## It decrypts every host, not just Anthropic

This is the part to be clear about. The proxy does not filter which
CONNECT requests it intercepts, so it terminates TLS for **every**
HTTPS host the subprocess reaches, decrypts the traffic, and
re-encrypts it upstream. The host checks described further down decide
what gets *rewritten*; they do not decide what gets decrypted.

Two consequences follow from that plus the fact that `HTTPS_PROXY` and
`HTTP_PROXY` are ordinary environment variables:

- Anything the `claude` subprocess spawns inherits them. When the CLI
  runs `gh`, `curl`, `npm` or anything else through its Bash tool, that
  process goes through forge's proxy too, and its TLS is terminated the
  same way.
- Every request body is read fully into memory before being forwarded,
  on every request to every host, including something like a `curl`
  file upload started from the Bash tool. Responses are the exception:
  they are streamed through untouched and never buffered, because
  `/v1/messages` is a server-sent-event stream and buffering it would
  stall the turn. `Content-Length` is recomputed on every forwarded
  request.

If you do not want a given account's traffic going through any of this,
set `proxy = false` on it; see [Turning it off](#turning-it-off).

## The certificate authority

The CA is generated on first launch and reused afterwards, so the path
handed to the subprocess stays stable. It is written to:

```
<app-support>/ca/ca-cert.pem
<app-support>/ca/ca-key.pem
```

where `<app-support>` is `~/Library/Application Support/forge-tui` on
macOS and `$XDG_DATA_HOME/forge-tui` on Linux. On Unix the key file is
chmod 0600, best-effort.

The certificate identifies itself. Its common name is
`forge wire-classification rewriter` and its organisation is
`forge-tui`. It is a CA certificate with unconstrained basic
constraints, valid from one hour before generation until 3650 days
after it, which is a couple of days short of ten calendar years. The
backdating absorbs clock skew on first run.

Generating the CA does not install it anywhere. Nothing trusts it by
default except the `claude` subprocess, which is pointed at it
explicitly through `NODE_EXTRA_CA_CERTS`.

### Trusting it system-wide, and why you might

`scripts/install-cert.sh` adds the CA to the macOS System keychain as a
trusted root. `just install` runs it best-effort at the end of an
install, and `just install-cert`, `just install-cert-status` and
`just install-cert-uninstall` drive it directly. The script is macOS
only.

The reason it exists is a consequence of how the proxy is wired up.
`HTTPS_PROXY` and `HTTP_PROXY` are ordinary environment variables, so
every process the `claude` subprocess itself spawns inherits them. When
the CLI runs `gh`, `curl`, `npm` or anything else through its Bash
tool, that tool's HTTPS traffic also goes through forge's proxy. Those
tools consult the system trust store rather than `NODE_EXTRA_CA_CERTS`,
so without the CA trusted there they fail with certificate errors.

Once the CA is in the System keychain, every process on the machine
that trusts the system store will accept certificates minted by it, and
the private key for that CA is a file in your home directory.
`just install` runs the script for you;
`just install-cert-uninstall` removes the CA again.

## What it rewrites

Decryption is universal, as above. Rewriting is not: what follows is
what changes in the bytes.

**On every outbound request, regardless of host**, the `User-Agent`
header is considered. It is only rewritten when the value carries a
parenthesised segment, which is the shape the CLI and forge's own MCP
client use; a plain agent string like `curl/8.4.0` passes through
untouched. That check is host-independent, so it also covers requests
to third-party MCP servers.

**On `anthropic.com` hosts**, additionally:

- one `anthropic-beta` flag is injected on exactly `/v1/messages`. The
  list of forge-only flags to strip is currently empty, so injection is
  the only change this makes,
- the bootstrap endpoint's query string is rewritten,
- the `/v1/messages` body is rewritten, including a classification
  substring the CLI bakes into the system prompt,
- event-logging and Statsig bodies are rewritten, and in the
  event-logging case whole entries whose event name carries the
  SDK-internal `tengu_sdk_` marker are dropped rather than normalised,
- any other Anthropic endpoint gets a catch-all pass of the same
  normaliser, so a new classification surface is covered without a
  code change.

**On `datadoghq.com` hosts**, the logs endpoint's body and tags are
rewritten, with the same `tengu_sdk_` entries dropped.

Normalisation runs in two passes. First a recursive walk over the
parsed JSON body, which anywhere at any depth:

- turns `entrypoint` and `client_type` string values that are not
  already `cli` into `cli`,
- turns `is_interactive` into `true` if it is anything else,
- removes `agent_sdk_version` keys.

Non-string values under `entrypoint` and `client_type` are left alone
rather than overwritten.

Then a byte-level pass over the serialised body, which short-circuits
unless the body contains `sdk-` somewhere. It is a fixed substring
replacement, not a general sweep: in practice it rewrites escaped
`\"entrypoint\":\"sdk-*\"` and `\"client_type\":\"sdk-*\"` for the four
known SDK labels, which is the form those fields take when they are
nested inside an already-serialised JSON string. Anything else carrying
`sdk-`, including an unescaped occurrence in a body that failed to
parse as JSON, is not covered by this pass.

The net effect is that forge sessions report themselves to Anthropic
and to Datadog as interactive CLI sessions.

### Requests answered locally

Two Anthropic paths are not forwarded at all. The proxy answers them
with a synthetic 200 response:

| Path contains | Response |
|---|---|
| `/v1/messages/count_tokens` | `{"input_tokens":0}` |
| `/claude_code/organizations/metrics_enabled` | `{"enabled":false}` |

The match is on the host ending in `anthropic.com` and the path
containing that substring. There is no method check, so a `GET` to
`/v1/messages/count_tokens` is stubbed just as a `POST` would be.

### When a rewrite fails

If rewriting a request errors, the proxy returns a 502 rather than
forwarding the unmodified original. The CLI surfaces that as a
transient API error.

### Drift detection

After rewriting, the proxy scans the outgoing body of every Anthropic
and Datadog request and logs a warning for anything that looks like a
classification signal it did not normalise: a string value starting
with `sdk-`, a non-`cli` `entrypoint` or `client_type`, a non-true
`is_interactive`, or a surviving `agent_sdk_version` key. A body on one
of those hosts that is not parseable JSON is also warned about, since
that would silently disable both the rewrite and the scan.

## Chaining through your own proxy

If `HTTPS_PROXY` or `https_proxy` is set in the environment when forge
launches, the rewriter routes its own outbound HTTPS through that
upstream proxy. An unparseable value there is one of the failures that
stops forge starting.

Independently of chaining, the rewriter always extends its outbound
trust store from `NODE_EXTRA_CA_CERTS`, falling back to `SSL_CERT_FILE`
if that is unset. A bundle it cannot read leaves it on the built-in
roots with a warning.

Between them, the same environment variables that let you capture
traffic from a bare `claude` invocation also capture forge's output.

## Turning it off

Per account, in `forge.toml`:

```toml
[[accounts]]
display_name = "Direct"
config_dir = "~/.claude-direct"
proxy = false
```

Sessions on that account get no proxy environment variables and talk to
Anthropic directly, carrying the CLI's own classification on the wire.

The proxy process itself still starts, and forge still refuses to boot
if it cannot. Setting `proxy = false` on every account does not disable
the startup requirement.

You can also override the proxy variables through a `forge.toml` env
table, since those writes are applied after forge's own. forge logs a
warning naming the key when you do, because it can stop the proxy from
seeing the child's traffic. See
[Environment layering](./configuration.md#environment-layering).
