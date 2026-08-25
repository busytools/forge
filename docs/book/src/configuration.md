# forge.toml reference

`forge.toml` is the only file forge reads for configuration. It is
hand-authored, forge never writes to it, and it is read once, at
startup. An edit therefore needs a forge restart to take effect: a
session spawned later in the same run still sees the value forge loaded
at boot.

## Where it lives

```
<config_dir>/forge/forge.toml
```

`<config_dir>` is `$CLAUDE_CONFIG_DIR` when that variable is set to a
non-empty value, and `$HOME/.claude` otherwise. If `$CLAUDE_CONFIG_DIR`
is unset and the home directory cannot be resolved, forge refuses to
launch rather than substituting a path derived from the directory it
was launched from.

If the file is absent, forge exits with:

```
forge.toml not found at <path>; create it with at least one [[orgs]]
entry containing one [[orgs.projects]] entry
```

## The shape

Two things are required: at least one org, and at least one account.
Orgs hold projects. Accounts describe the `claude` config directories
forge can spawn a session under. An org names the subset of accounts
its projects are allowed to use.

Everything else is optional.

## `[[orgs]]`

An array of tables. At least one is required, or the load fails with
`no [[orgs]] entries in forge.toml`.

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Must be unique across orgs. |
| `accounts` | array of strings | yes | Each entry must match an `[[accounts]]` `display_name`. |
| `projects` | array of tables | yes | Written as `[[orgs.projects]]`. An org with none fails the load. |

`accounts` is the account subset every project in this org may spawn
under. Rules enforced at load:

- An empty list (`accounts = []`) fails with `has an empty
  accounts = [] list`.
- A name that matches no `[[accounts]]` entry fails, and the error
  lists the valid names.
- A list containing only accounts marked `experimental = true` fails,
  because such an org would leave its projects with nothing to spawn
  under.

## `[[orgs.projects]]`

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `name` | string | yes | | Must be unique across *all* orgs, not just within one. |
| `path` | string | yes | | A leading `~/` is expanded to the home directory. The un-expanded string is kept for display. |
| `auto_start` | bool | no | `false` | When true, the project's lead session spawns at forge launch. Any number of projects may set it. |
| `static_workers` | array of strings | no | `[]` | Worker role labels to spawn alongside this project's lead. |

Project names are the argument `forge <PROJECT>` takes, and the key
`[projects.<name>]` env tables refer to.

### `static_workers`

Each label resolves to a charter and an initial kick prompt on disk.
Labels are validated at load. The files are read at startup, when forge
back-fills a stored worker record for any label that has none, so a
label naming a role you have not created yet loads fine and is skipped
there. From then on the stored record is what spawns the worker, and
editing the files no longer changes it.

Label validation at load rejects: an empty label, a leading `/`, and
any path segment equal to `.`, `..`, or empty (which a `//` would
produce). A `/` is otherwise allowed as a namespace separator, so
`researcher` and `some-namespace/researcher` are both well-formed.
Repeating a label within one project is also rejected: one instance per
label per project.

At back-fill, a label is resolved project-scoped first and globally second.
For a project named `P`, forge looks for
`~/.claude/forge-team/P/<label>/charter.md` and then
`~/.claude/forge-team/<label>/charter.md`, and uses the first that
exists. Each role directory needs both `charter.md` and `kick.md`. A
label whose files resolve nowhere is skipped with a warning rather than
blocking the rest of the roster.

That root comes from the home directory, not from `<config_dir>`, so it
stays at `~/.claude/forge-team/` even when `$CLAUDE_CONFIG_DIR` points
somewhere else.

You author these files yourself; forge ships no starting content and
does no bootstrap. The `lead` role is the exception: when
`~/.claude/forge-team/lead/charter.md` is absent, forge falls back to a
charter compiled into the binary, so a lead is always charter-backed.

## `[[accounts]]`

An array of tables. At least one is required, or the load fails with
`no [[accounts]] entries in forge.toml`.

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `display_name` | string | yes | | Must be unique. This is the name orgs reference. |
| `config_dir` | string | yes | | The `claude` config directory this account uses. `~/` is expanded. |
| `proxy` | bool | no | `true` | Whether to attach the [wire-classification proxy](./classification-proxy.md) to this account's sessions. |
| `experimental` | bool | no | `false` | Excludes the account from automatic assignment while leaving it selectable by hand. |
| `env` | table | no | `{}` | Written as `[accounts.env]`. See [Environment layering](#environment-layering). |

`config_dir` is what forge exports as `CLAUDE_CONFIG_DIR` to the
spawned `claude` subprocess, so each account reads and writes its own
credentials, session history and settings tree.

`proxy = false` means the spawned `claude` talks to Anthropic directly
and its wire signals carry the CLI's own classification. The proxy
itself still starts at boot regardless of this setting; see the
[proxy page](./classification-proxy.md).

## `[env]`

A flat table of string keys to string values, stamped onto every
spawned `claude` subprocess. Absent means empty.

```toml
[env]
CLAUDE_CODE_AUTO_COMPACT_WINDOW = "950000"
```

## `[projects.<name>]`

Per-project environment, keyed by a project's `name`. Unlike the
top-level tables, this one rejects unknown fields, so a mistyped inner
table fails the load loudly instead of quietly applying nothing.

| Key | Type | Default | Notes |
|---|---|---|---|
| `env` | table | `{}` | Written as `[projects.<name>.env]`. |
| `env_file` | string | none | Path to a `KEY=value` file whose entries join this project's env. |

A `[projects.<name>]` block naming a project that no
`[[orgs.projects]]` declares fails the load, and the error lists the
declared project names. That is deliberate: the name is repeated by
hand here, so a typo would otherwise land nowhere silently.

### `env_file`

The file is parsed as `KEY=value` lines. Blank lines and lines starting
with `#` are skipped. One matching pair of surrounding single or double
quotes is stripped from the value; an unmatched quote stays part of the
value.

Failures here are non-fatal and warn rather than refusing to boot:

- A relative path is skipped entirely. Only absolute or `~/` paths are
  read, because a relative path would resolve differently depending on
  where forge was launched from.
- A missing or unreadable file contributes no keys.
- A line with no `=` is skipped; the rest of the file still applies.

The inline `[projects.<name>.env]` table wins over `env_file` per key.

## Environment layering

Three layers merge per key, narrowest winning:

```
[env]  <  [accounts.env]  <  [projects.<name>.env]
```

The global and account layers merge at load. The project layer is
applied at spawn rather than earlier, because one account serves many
projects and merging sooner would leak one project's keys into every
other project on that account.

Four keys are reserved by forge: `CLAUDE_CONFIG_DIR`, `HTTPS_PROXY`,
`HTTP_PROXY` and `NODE_EXTRA_CA_CERTS`. Setting one of these in any env
layer overrides forge's own stamp, which can stop the
wire-classification proxy from seeing the child's traffic. The value
still applies, since `forge.toml` is treated as trusted, but forge logs
a warning naming the key.

An `ANTHROPIC_BASE_URL` under `[accounts.env]` is how an account points
at an alternate endpoint; there is no dedicated field for it, and
forge's usage probe reads it from there. Setting `ANTHROPIC_BASE_URL`
or `ANTHROPIC_AUTH_TOKEN` at the *project* layer instead desynchronises
forge's own accounting, because the usage probe, plan detection and the
account picker all read the account map.

Only key names, never values, are recorded in forge's per-spawn log
line. These tables hold tokens.

## `[ui]`

Optional. Every field has a default, so an absent section is the same
as all defaults.

| Key | Type | Default | Accepted values |
|---|---|---|---|
| `spinner` | string | `braille` | `braille`, `phase_of_moon`, `ember`, `bars_v`, `star`, `sparkle` |
| `fps` | integer | `120` | 30 to 240 |

Both keys are lenient, so a hand-edited typo does not stop forge
booting. A `spinner` name forge does not recognise resolves to the
default. An `fps` outside the range is clamped and warned about, and a
non-integer `fps` resolves to the default.

`launchpad_spinner` is accepted as an alias for `spinner`.

The spinner set here is the default. A `/spinner` pick made inside
forge is persisted separately and wins over it.

## `[gotify]`

Optional. Absent means the Gotify integration stays dormant.

| Key | Type | Required | Notes |
|---|---|---|---|
| `url` | string | yes | Gotify server URL. |
| `client_token` | string | yes | Token for the receive stream and the application lookup. |

Both are mandatory once the section is present; neither has a default.

## Unknown keys

The top-level document does not reject unknown tables, so a section
forge no longer reads is ignored rather than failing the load. The one
place that does reject unknown fields is `[projects.<name>]`.

## A complete example

```toml
# Applies to every spawned session, unless a narrower layer overrides
# the same key.
[env]
CLAUDE_CODE_AUTO_COMPACT_WINDOW = "950000"

[[orgs]]
name = "Personal"
accounts = ["Personal", "Scratch"]

  [[orgs.projects]]
  name = "forge"
  path = "~/Projects/forge"
  auto_start = true
  static_workers = ["planner", "implementer", "reviewer"]

  [[orgs.projects]]
  name = "notes"
  path = "~/Projects/notes"

[[orgs]]
name = "Work"
accounts = ["Work"]

  [[orgs.projects]]
  name = "service"
  path = "~/Projects/service"

[[accounts]]
display_name = "Personal"
config_dir = "~/.claude"

[[accounts]]
display_name = "Work"
config_dir = "~/.claude-work"

# Talks to a local endpoint, so the classification proxy adds nothing.
[[accounts]]
display_name = "Scratch"
config_dir = "~/.claude-scratch"
proxy = false
experimental = true

  [accounts.env]
  ANTHROPIC_BASE_URL = "http://localhost:18765"
  ANTHROPIC_AUTH_TOKEN = "unused"

# Per-project env, keyed by the project's `name`.
[projects.service]
env_file = "~/.config/service/secrets.env"

  [projects.service.env]
  SERVICE_MCP_URL = "https://mcp.example/service"

[ui]
spinner = "phase_of_moon"
fps = 120

[gotify]
url = "https://gotify.example"
client_token = "CxxxxxxxxxxxxxxxA"
```

## What forge does at startup

`forge <PROJECT>` opens the named project. The name must match a
project's `name` exactly.

With no argument, forge opens the launchpad picker instead of a chat
tab. Every project carrying `auto_start = true` still spawns its lead
session in the background, but none of them is focused: you pick one
from the launchpad to enter chat.

`auto_start` therefore controls what is warm when you arrive, not what
you land on.

## Config versus state

`forge.toml` is the config half. It is read-only from forge's point of
view, which makes it safe to sync between machines.

Everything mutable lives in a single embedded redb database at
`<app-support>/db.redb`: durable crons, Gotify subscriptions, dynamic
workers spawned at runtime, review threads, the `/spinner` override,
the per-account usage cache, cached model pricing, and the `/usage`
view's per-file token summaries.

`<app-support>` is `~/Library/Application Support/forge-tui` on macOS
and `$XDG_DATA_HOME/forge-tui` on Linux.

The single-instance lock lives under the same base, at
`<app-support>/locks/<hash>.lock`, where the hash is derived from the
config directory path.

Neither belongs in a synced config directory. The database churns
constantly and redb's binary file cannot be merged by a file syncer.
The lock is worse: `flock` binds to the inode rather than the path, so
a sync tool that replaces the file by rename would swap the lock out
from under a running forge on another machine, and a second instance
could then start against the same config directory.

One forge process owns one config directory. A second is normally
refused at boot, naming the holder's PID when it can read one. The
guard is best-effort: if the lockfile cannot be created or locked,
forge warns and starts anyway. See
[Single instance per config directory](./architecture.md#single-instance-per-config-directory).
