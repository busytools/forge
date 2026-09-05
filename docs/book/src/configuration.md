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

Project names are the argument `forge <PROJECT>` takes, and the key
`[projects.<name>]` env tables refer to.

## `[[accounts]]`

An array of tables. At least one is required, or the load fails with
`no [[accounts]] entries in forge.toml`.

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `display_name` | string | yes | | Must be unique. This is the name orgs reference. |
| `config_dir` | string | yes | | The `claude` config directory this account uses. `~/` is expanded. |
| `provider` | string | yes | | One of `"anthropic"`, `"codex"`, `"openrouter"`, `"zai"`. Decides how the account is probed and how its usage reads. |
| `experimental` | bool | no | `false` | Excludes the account from automatic assignment while leaving it selectable by hand. |
| `permission_mode` | string | no | | Stamps the CLI's permission mode onto every session this account spawns, overriding the session default. |
| `env` | table | no | `{}` | Written as `[accounts.env]`. See [Environment layering](#environment-layering). |

`config_dir` is what forge exports as `CLAUDE_CONFIG_DIR` to the
spawned `claude` subprocess, so each account reads and writes its own
credentials, session history and settings tree.

`provider` has no default. Accounts that omit it are named together in
one load error listing the accepted values, so a first run does not
surface them one restart at a time. Silence is the dangerous answer
here: a mislabelled account probes the wrong endpoint and then cannot
reach a usable state, which stops forge starting.

The line has to sit above the account's `[accounts.env]` table. TOML
scopes every key after a table header into that table, so a `provider`
written below one is read as an environment variable and the account
still counts as missing it.

`"anthropic"` reads credentials from the macOS keychain and probes the
default host - unless its env carries `CLAUDE_CODE_OAUTH_TOKEN` (from
`[accounts.env]` or the global `[env]`), in which case that token is
the credential and the keychain is not read (see [Environment
layering](#environment-layering)). `"codex"`, `"openrouter"` and
`"zai"` authenticate with the `ANTHROPIC_AUTH_TOKEN` beside their
`ANTHROPIC_BASE_URL`, and an account declaring any of them without
that base url fails the load naming the account and the missing key.
Either key may come from the account's own `[accounts.env]` or from
the global `[env]`, since the two are merged before the check runs.

For `"openrouter"` the base url must be the API root, `https://openrouter.ai/api`,
and an account whose base does not end in `/api` fails the load. The
bare host is the trap worth naming: forge probes `{base}/v1/key`, and
`https://openrouter.ai/v1/key` answers `200` with a web page rather than
a `404`, so nothing downstream could tell it apart from a real reply.

The split is billing, not auth. `"codex"` is a base-url account whose
proxy serves the same windowed body Anthropic does, so it reads as a
subscription with rolling windows. `"zai"` is a subscription too: its
usage is probed at the monitor host derived from the base url's host
root and read as rolling 5-hour and weekly credit windows. `"openrouter"`
is pay-per-token: there is no window, so its usage is money spent over
a period rather than a percentage of a plan. A key may carry a spending
cap set from the provider's dashboard, and where it does forge reads the
spend against it as a percentage too; an uncapped key has no
denominator, and forge says so rather than showing an empty bar.

Unknown keys in an `[[accounts]]` block are rejected, so a near-miss
like `providers` fails the load instead of loading and doing nothing.

`permission_mode` stamps a permission mode onto every session the
account spawns, overriding the launcher's per-session default, so one
account can run bypassed while the rest keep the session default. The
key lives on the account rather than a project because the account owns
the CLI's credential and endpoint, so it owns the mode. The accepted
values are the CLI's mode names, `"default"`, `"acceptEdits"`, `"plan"`,
`"dontAsk"`, `"auto"` and `"bypassPermissions"`, plus the legacy aliases
`"ask"`, `"deny"`, `"accept_edits"`, `"dont_ask"` and
`"bypass_permissions"`; anything else fails the load listing them. The
mode the session actually runs is what the CLI reports back on connect,
and the `/mode` picker offers `bypassPermissions` only on sessions
launched into it; the CLI refuses a mid-session switch to bypass.

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

One key is reserved by forge: `CLAUDE_CONFIG_DIR`. Setting it in any
env layer overrides forge's own stamp. The value still applies, since
`forge.toml` is treated as trusted, but forge logs a warning naming the
key.

An `ANTHROPIC_BASE_URL` under `[accounts.env]` is where a `"codex"`,
`"openrouter"` or `"zai"` account's endpoint lives, alongside the
`ANTHROPIC_AUTH_TOKEN` it authenticates with. It does not decide how
the account is probed - `provider` does, and this is only read once
that has already chosen a base-url account. Setting `ANTHROPIC_BASE_URL`
or `ANTHROPIC_AUTH_TOKEN` at the *project* layer instead desynchronises
forge's own accounting, because the usage probe, plan detection and the
account picker all read the account map.

A `CLAUDE_CODE_OAUTH_TOKEN` in an `"anthropic"` account's env - its
own `[accounts.env]`, or the global `[env]` every account extends -
makes the account token-mode: the token, minted by
`claude setup-token`, is the credential, the keychain is never read,
and several accounts can share one config dir. The usage endpoint
refuses setup tokens (they lack the `user:profile` scope), so a valid
token is probed with a minimal billed messages call instead - its
response headers carry the same 5-hour and 7-day usage windows
keychain accounts render, at roughly nine tokens per account per
usage poll; a rejected token renders as an auth failure whose repair
is a re-mint. Like every env key, it is read once at boot, so
replacing the token needs a restart.

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

## `[dictate]`

Optional. Absent means dictation is off, which is also what an explicit
`enabled = false` means.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | boolean | `false` | Off unless asked for. Turning it on costs a 3.07 GB model download the first time and holds about 1.8 GB of resident memory for the run. |
| `models_dir` | string | platform cache dir | Where the model files live. `~` is expanded. |
| `device` | string | system default | Input to record from, by device id rather than name. |
| `language` | string | autodetect | Spoken language hint. |
| `normalizer` | boolean | `true` | Rewrite recognition output into clean text. Off halves the download and skips a pass per utterance. |
| `max_capture_minutes` | integer | `30` | Upper bound on one recording. A capture reserves memory eagerly, about 110 MiB at the default. |
| `bind` | string | `right_cmd` | The push-to-talk key: `right_cmd`, `left_cmd` or `off`. On Linux and Windows the cmd equivalent is the right/left Control key. |
| `mode` | string | `auto` | How press/release maps onto recording: `auto` infers from timing (a quick tap toggles, a hold transcribes on release), `toggle` starts on a press and stops on the next press, `hold` records while held and always transcribes on release. |

Unlike `[ui]`, an unrecognised key here fails the load rather than being
ignored: a mistyped `models_dir` would otherwise fetch three gigabytes
to the wrong volume with nothing said about it.

With `enabled = true`, forge fetches, verifies and loads the models on
the preflight screen before forge hands over. A first run
downloads 3.07 GB, resumable and SHA-256 verified; later runs re-hash
what is on disk, which takes a few seconds, then load the weights.
Pressing `esc` during a download keeps what has landed and quits.

The model files themselves are not configurable. Each carries a URL, a
byte length and a digest, and a hand-edited one is a file nothing can
verify.

## `[gotify]`

Optional. Absent means the Gotify integration stays dormant.

| Key | Type | Required | Notes |
|---|---|---|---|
| `url` | string | yes | Gotify server URL. |
| `client_token` | string | yes | Token for the receive stream and the application lookup. |

Both are mandatory once the section is present; neither has a default.

## `[plugins]`

Optional. Absent means plugin auto-update is off, which is also what an
explicit `auto_update = false` means. Manual updates from the plugins
pane (`u`, or the per-plugin Update action) are not affected by this
section.

| Key | Type | Default | Notes |
|---|---|---|---|
| `auto_update` | boolean | `false` | Update every installed plugin once at forge boot. The switch alone governs. Off unless asked for: an auto-applied update can break a load-bearing session mid-day. |

When the run fires, the plugins pane reports what updated, from which
marketplace, and what it skipped; forge remembers the previous version
so the plugin's actions overlay can offer "Roll back to previous
version" afterwards. Rollback needs the recorded pre-update marketplace
ref, which in turn needs the marketplace to be git-backed, and the ref
to still be fetchable; a rollback that does not actually move the
plugin to the recorded version keeps the record so it can be retried.

Like `[dictate]`, an unrecognised key here fails the load rather than
being ignored. Keys an older forge read here (`trusted_marketplaces`,
`pins`) are rejected the same way: remove them.

## Unknown keys

The top-level document does not reject unknown tables, so a section
forge no longer reads is ignored rather than failing the load. The
places that do reject unknown fields are `[[accounts]]`,
`[projects.<name>.env]`, `[dictate]` and `[plugins]`.

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
provider = "anthropic"

[[accounts]]
display_name = "Work"
config_dir = "~/.claude-work"
provider = "anthropic"

# Talks to a local endpoint. The provider line has to come before
# [accounts.env], or TOML reads it as an env key.
[[accounts]]
display_name = "Scratch"
config_dir = "~/.claude-scratch"
provider = "codex"
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

[dictate]
enabled = true
normalizer = true
max_capture_minutes = 30

[gotify]
url = "https://gotify.example"
client_token = "CxxxxxxxxxxxxxxxA"

[plugins]
auto_update = true
```

## What forge does at startup

`forge <PROJECT>` opens the named project. The name must match a
project's `name` exactly.

**Preflight runs first on every route.** It resolves every
`[[accounts]]` entry and, when `[dictate]` is on, fetches and loads the
dictation models, then hands over to wherever you were headed: the
project picker for `forge`, straight into that project's chat for
`forge <PROJECT>`. It is shown once per run.

Nothing spawns until every account has authenticated, because the
account-assignment plan is only computed once they have.

Preflight completes only on every account reaching a usable state.
**forge will not start while an account in `forge.toml` cannot
authenticate** - fix that account's auth, or remove its `[[accounts]]`
block. The screen names both.

Every project carrying `auto_start = true` still spawns its lead session
in the background, but none of them is focused: you pick one from the
picker to enter chat.

`auto_start` therefore controls what is warm when you arrive, not what
you land on.

## Config versus state

`forge.toml` is the config half. It is read-only from forge's point of
view, which makes it safe to sync between machines.

Everything mutable lives in a single embedded redb database at
`<app-support>/db.redb`: durable crons, Gotify subscriptions, dynamic
workers spawned at runtime, review threads, the `/spinner` override,
the per-account usage cache, cached model pricing, cached OpenRouter
model catalogs, and the `/usage` view's per-file token summaries.

The one counterexample is dictation diagnostics: with dictation
enabled, each take's audio and transcripts are kept as plain files
under `<app-support>/dictate-diagnostics/` - voice recordings outside
the database - with the same machine-local, never-synced caveat.

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
