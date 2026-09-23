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
Orgs hold projects. Accounts are the provider endpoints and
credentials forge can spawn a session under. An org names the subset
of accounts its projects are allowed to use.

Everything else is optional.

## `[[orgs]]`

An array of tables. At least one is required, or the load fails with
`no [[orgs]] entries in forge.toml`.

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Must be unique across orgs. |
| `accounts` | array of strings | yes | Each entry must match an `[[accounts]]` `display_name`. |
| `fallback_accounts` | array of strings | no | Accounts the walk reaches after the pinned ones. Absent means none. |
| `projects` | array of tables | yes | Written as `[[orgs.projects]]`. An org with none fails the load. |

`accounts` is the account subset every project in this org may spawn
under. Rules enforced at load:

- An empty list (`accounts = []`) fails with `has an empty
  accounts = [] list`.
- A name that matches no `[[accounts]]` entry fails, and the error
  lists the valid names.

`fallback_accounts` is walked after the org's `accounts`: a fallback
declaring the project's model outranks a saturated or bailed primary,
and the primary is taken again once it heals. The names also render in
the `/gateway` view's fallback pin. The list is validated at load
exactly like `accounts`: a name matching no `[[accounts]]` entry fails
the boot, naming the account and the valid names. An account may appear
in both lists; it is then a primary only. See [the launchpad's chip
description](./ui/launchpad.md) for the walk order.

`accounts` and `fallback_accounts` must also agree on any env that
changes the *shape* of a request, not only where it is sent. A child's
env is frozen when its session spawns and the gateway never respawns
it, so after a rotation the next account is handed a request shaped for
the account the session started on. The live example is
`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS`: OpenRouter sets it and Zai
leaves it unset, both declare `glm-5.3-flash`, and OpenRouter rejects
the beta shape the flag suppresses. A project declaring `glm-5.3-flash`
in an org holding both would spawn on Zai and then fail every rotation
onto OpenRouter, with a restart the only way out.

## `[[orgs.projects]]`

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `name` | string | yes | | Must be unique across *all* orgs, not just within one. |
| `path` | string | yes | | A leading `~/` is expanded to the home directory. The un-expanded string is kept for display. |
| `auto_start` | bool | no | `false` | When true, the project's lead session spawns at forge launch. Any number of projects may set it. |

Project names are the argument `forge <PROJECT>` takes, and the key
per-project settings hang off.

## `[[accounts]]`

An array of tables. At least one is required, or the load fails with
`no [[accounts]] entries in forge.toml`.

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `display_name` | string | yes | | Must be unique. This is the name orgs reference. |
| `provider` | string | yes | | One of `"anthropic"`, `"codex"`, `"openrouter"`, `"zai"`. Decides how the account is probed and how its usage reads. |
| `token` | string | yes | | The credential the gateway forwards, mapped onto the provider's own variable at spawn. Trimmed once at load. |
| `base_url` | string | no | | The upstream base for base-url providers (`"codex"`, `"openrouter"`, `"zai"`); required for them and validated per provider. An `"anthropic"` account may omit it - the gateway constant is its upstream. |
| `models` | list of strings | yes | | The canonical model names the account serves. Selection and the route's model gate match against this list; an empty list fails the load (`AccountModelsRequired`). |
| `model_slugs` | table | no | `{}` | Canonical name -> upstream spelling, only where they differ. Every slug key must be in `models`, or the load fails (`AccountSlugUndeclared`). |
| `model_aliases` | table | no | `{}` | Written as `[accounts.model_aliases]`. Canonical name -> the other names a request may arrive under for that model. Every key must be in `models`, or the load fails (`AccountAliasUndeclared`); an empty list fails (`AccountAliasEmpty`). |
| `env` | table | no | `{}` | Written as `[accounts.env]`. Provider-behaviour extras only - timeouts, context caps, fallback switches. Gateway keys (`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`) declared here or in the global `[env]` layer are dropped at load and warned about: the flat `base_url` and `token` keys own them, and forge reads the account's endpoint and credential from those. |

All accounts share one `claude` config directory, so MCP servers,
plugins and settings are declared once for every account; what varies
per account is exactly the flat block above.

`model_aliases` covers the case where the name the gateway sees is not
the name you configured. Long-context models are the usual reason: the
`claude` CLI is handed `claude-opus-5[1m]`, and the request body it
then sends carries the plain `claude-opus-5`. Matching the body's model
against `models` alone would refuse that request, so the plain spelling
is declared as an alias of the marker. A request naming any member of a
model's set - the declared name or any alias of it - matches the
account, so one account serves both spellings. forge never interprets
the names; an alias is matched as data.

`provider` has no default. Accounts that omit it are named together in
one load error listing the accepted values, so a first run does not
surface them one restart at a time. Silence is the dangerous answer
here: a mislabelled account probes the wrong endpoint and then cannot
reach a usable state, which stops forge starting.

The line has to sit above the account's `[accounts.env]` table. TOML
scopes every key after a table header into that table, so a `provider`
written below one is read as an environment variable and the account
still counts as missing it.

`"anthropic"` accounts may omit `base_url`: the gateway constant is
their upstream. `"codex"`, `"openrouter"` and `"zai"` require it, and
an account declaring any of them without a `base_url` fails the load
naming the account and the missing key.

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

## `[env]`

A flat table of string keys to string values, stamped onto every
spawned `claude` subprocess. Absent means empty.

```toml
[env]
CLAUDE_CODE_AUTO_COMPACT_WINDOW = "950000"
```

## Per-project keys (inside `[[orgs.projects]]`)

The per-project settings live directly on each `[[orgs.projects]]`
entry, beside `name`, `path` and `auto_start`. The entry rejects
unknown fields, so a mistyped key fails the load loudly instead of
quietly applying nothing.

| Key | Type | Default | Notes |
|---|---|---|---|
| `env` | table | `{}` | Written as `[orgs.projects.env]` directly under the entry, or inline as `env = { ... }`. The block form attaches to the MOST RECENT `[[orgs.projects]]` header - place it directly under its own entry, before the next header, or it lands on a different project. |
| `env_file` | string | none | Path to a `KEY=value` file whose entries join this project's env. |
| `max_workers` | integer | `2` | Cap on this project's concurrently live dynamic workers. The count is per project: workers live in other projects neither consume this project's budget nor raise its cap. A spawn over the cap errors instead of queuing; despawning a worker frees its slot. Workers restored by the boot or lead-reconnect respawn of persisted rows are exempt, but still count toward the cap once live. `0` disables dynamic spawns for the project. |
| `permission_mode` | string | `auto` | Stamps the CLI's permission mode onto every session this project spawns, overriding the session default. Absent means `auto`, not the session default, so a project's sessions run one mode however its org's accounts rotate. |
| `model` | string | | The project's model. Fills the CLI's model slots at spawn (`ANTHROPIC_DEFAULT_HAIKU_MODEL`, `_OPUS_MODEL`, `_SONNET_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL` all carry it) and is the session default a `/model` change overrides. Must be declared by at least one account in the org, or the load fails - a model no account serves would otherwise stamp every slot and 503 each session's first request. Required: the walk that picks a session's account matches on the model an account serves, so a project without one is refused at spawn. |

`permission_mode` stamps a permission mode onto every session the
project spawns, overriding the launcher's per-session default. A project
that does not set the key gets `auto` stamped rather than the launcher
default, so its sessions run one mode consistently however the org's
accounts rotate. The accepted values are the CLI's mode names,
`"default"`, `"acceptEdits"`, `"plan"`, `"dontAsk"`, `"auto"` and
`"bypassPermissions"`, plus the legacy aliases `"ask"`, `"deny"`,
`"accept_edits"`, `"dont_ask"` and `"bypass_permissions"`; anything else
fails the load listing them. The mode the session actually runs is what
the CLI reports back on connect, and the `/mode` picker offers
`bypassPermissions` only on sessions launched into it; the CLI refuses a
mid-session switch to bypass.



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

The inline `env` table on the project entry wins over `env_file` per key.

## `[gateway]`

The inference listener that spawns resolving to a project are pointed
at.

| Key | Type | Default | Notes |
|---|---|---|---|
| `port` | integer | `8787` | The port the gateway's listener binds on `127.0.0.1`. There is no fallback to an OS-assigned port: if the port is taken, the gateway fails to start and preflight stays shut, because every session's base URL names this port and a silent drift would point children at an address nothing serves. `0` fails the load outright (`GatewayPortInvalid`) for the same reason - it reads as "pick one for me". |
| `streak_count` | integer | `5` | How many consecutive 429s from one account fire a rotation. `0` fails the load outright (`GatewayRotationInvalid`) - it would silently disable the streak. |
| `streak_window_secs` | integer | `60` | The window the 429 streak is counted over, in seconds. `0` fails the load outright (`GatewayRotationInvalid`). |
| `no_reset_cooldown_secs` | integer | `60` | The cooldown applied when neither the failing response nor the account's own usage probe reports a reset time, in seconds. `0` fails the load outright (`GatewayRotationInvalid`). |

## Environment layering

Three layers merge per key, narrowest winning:

```
[env]  <  [accounts.env]  <  [[orgs.projects]] env
```

A project's env block attaches to the most recent `[[orgs.projects]]`
header - place it directly under its own entry.

The global and account layers merge at load. The project layer is
applied at spawn rather than earlier, because one account serves many
projects and merging sooner would leak one project's keys into every
other project on that account.

One carve-out applies to project spawns: after those layers compose,
the gateway re-stamps four keys over the result - `ANTHROPIC_BASE_URL`
(the listener with the session's routing segments), the
`CLAUDE_CODE_API_BASE_URL` slot, the account's credential variable, and
`ANTHROPIC_API_KEY` (forced empty) - so no layer can point a child away
from the listener while it holds only the dummy credential.

One key is reserved by forge: `CLAUDE_CONFIG_DIR`. Setting it in any
env layer overrides forge's own stamp. The value still applies, since
`forge.toml` is treated as trusted, but forge logs a warning naming the
key.

A base-url account's endpoint and credential are its flat `base_url`
and `token` keys, mapped onto the CLI's variable names at load.
`[accounts.env]` carries only provider-behaviour extras - timeouts,
context caps, fallback switches. A base-url or credential key declared
in an env layer is dropped at load, so it can reach neither the child
nor the pool the usage probe and the forward leg read: the flat keys
own them. Setting `ANTHROPIC_BASE_URL` or `ANTHROPIC_AUTH_TOKEN` at the
*project* layer instead desynchronises forge's own accounting, because
the usage probe, plan detection and the `/gateway` view all read the
account map.

An `"anthropic"` account's flat `token` - minted by
`claude setup-token` - is its credential. The usage endpoint
refuses setup tokens (they lack the `user:profile` scope), so a valid
token is probed with a minimal billed messages call instead - its
response headers carry the 5-hour and 7-day usage windows
at roughly nine tokens per account per
usage poll; a rejected token renders as an auth failure whose repair
is a re-mint. Like every key, it is read once at boot, so
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

The two values are lenient, so a hand-edited typo does not stop forge
booting. A `spinner` name forge does not recognise resolves to the
default. An `fps` outside the range is clamped and warned about, and a
non-integer `fps` resolves to the default. The keys are not lenient: an
unrecognised key in this section fails the load like any other.

Forge writes an OSC 777 desktop-notification escape every time it
raises a notification, and asks nothing about the terminal first. A
terminal that ignores the escape is harmless, so nothing is planned
around the answer. Notifications are raised only while forge reads the
terminal window as unfocused, and that focus signal is relayed and can
lag the actual frontmost state.

The escape carries the session as its title field and the event as its
body, so the banner's bold line names the session instead of the app: a
lead's is the project alone, a worker's is the project with the worker's
label in brackets after it. The line under it is the terminal's own
window title, which forge sets separately as the tab title.

What crosses is decided by whatever sits between forge and the
terminal, and no setting changes it. Ghostty with no multiplexer
renders the banner, and so does Ghostty through shpool: shpool does
not carry `TERM_PROGRAM` into the pane but forwards the escape
(measured 2026-09-13, with both the `ST` and `BEL` terminators). Under
zellij and GNU screen an escape emitted inside does not reach the
outer pty, measured 2026-08-29 against OSC 9 with the 777 form
unmeasured there, so the banner is not expected to appear. tmux
re-emits only the forms its terminfo carries - OSC 8 and the OSC 9;4
progress bar - so an OSC 777 notification is not forwarded, and tmux
substitutes `TERM_PROGRAM` and `TERM` with its own values besides.
When the banner does arrive it shows while Ghostty is not the
frontmost app, and is downgraded to a dock bounce when it is.

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

## `[[slack]]`

Optional, and repeatable: one entry per Slack workspace. Absent or empty
means the Slack connector stays dormant.

| Key | Type | Default | Notes |
|---|---|---|---|
| `workspace` | string | none | Label for this workspace, distinct per entry. It addresses the workspace in `slack__list`. |
| `token` | string | none | User token, `xoxp-...`. |
| `poll_seconds` | integer | `5` | Sweep interval for this workspace, in seconds. Slack's allowance is per workspace and a `direct_messages` subscription costs one history call per DM it covers, so a large inbox wants a longer interval here. |
| `thread_idle_days` | integer | `14` | Drop a followed thread with nothing new for this many days. A thread that quiet is resolved in practice, whatever its parent's age. |

`workspace` and `token` are mandatory once an entry is present. Three
mistakes fail the load rather than booting a connector that cannot work:
an empty `workspace`, an empty `token`, and two entries sharing a
`workspace` label. An unknown key inside an entry is rejected, so a
near-miss fails loudly.

The token is a credential, so it lives here rather than in the state
store beside the subscriptions. forge proves it with `auth.test` at boot
and logs the team and user it resolves to, or the failure; a workspace
whose token fails keeps its conversation sweeps running but its mention
stream stays down - the sweeps cannot recognise `<@U...>` without it -
and forge retries the proof in the background until it succeeds.

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

Every table rejects unknown fields, so a mistyped key fails the load
and names itself rather than parsing clean and meaning something else -
a misspelled `fallback_accounts` would otherwise read as "no
fallbacks". That covers the top level, `[[orgs]]`, `[[orgs.projects]]`,
`[[accounts]]`, `[[slack]]`, `[gotify]`, `[ui]`, `[gateway]`,
`[dictate]` and `[plugins]`.

A key forge itself retired is a declared ghost rather than an unknown
key, so a stale `forge.toml` still boots and warns instead of failing:
`[workers]`, `[projects.<name>]`, `[selection]` and
`[ui] notifications_osc9`. Anything else in those places is a typo and
is refused.

The one deliberate exception to the refusal is `[ui]`'s two values,
`spinner` and `fps`: an unrecognised spinner name and an out-of-range
`fps` resolve to defaults instead of failing the load.

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
  max_workers = 4
  env_file = "~/.config/service/secrets.env"

  [orgs.projects.env]
  SERVICE_MCP_URL = "https://mcp.example/service"

[[accounts]]
display_name = "Personal"
token = "personal-setup-token"
models = ["claude-opus-5", "claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Work"
token = "work-setup-token"
models = ["claude-opus-5", "claude-sonnet-5"]
provider = "anthropic"

# Talks to a local endpoint: the flat base_url and token keys carry
# the endpoint and credential; [accounts.env] stays for
# provider-behaviour extras only.
[[accounts]]
display_name = "Scratch"
provider = "codex"
base_url = "http://localhost:18765"
token = "scratch-token"
models = ["claude-sonnet-5"]

  [accounts.env]
  CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS = "1"

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

[[slack]]
workspace = "acme"
token = "xoxp-xxxxxxxxxxxx"
poll_seconds = 5
thread_idle_days = 14

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

Nothing spawns until every account has settled, because the walk that
picks a session's account reads each one's state.

**A bailed account does not stop forge from starting.** `Ready` and
`Bailed` both count as settled, so preflight completes: the row names
the failure and the pollers keep re-probing it. The walk still keeps a
bailed account as a last resort, and picks one when nothing else in the
pin declares the project's model - which is why the project row stays
clickable. Fix that account's auth, or remove its `[[accounts]]` block,
then restart forge to pick the edit up. The screen names both.

Every project carrying `auto_start = true` still spawns its lead session
in the background, but none of them is focused: you pick one from the
picker to enter chat.

`auto_start` therefore controls what is warm when you arrive, not what
you land on.

## Config versus state

`forge.toml` is the config half. It is read-only from forge's point of
view, which makes it safe to sync between machines.

Everything mutable lives in a single embedded redb database at
`<app-support>/db.redb`: durable crons, Gotify subscriptions, Slack
subscriptions and the sweep watermarks beside them, the session
identity per project and label - the lead's and every worker's, which is
what brings a worker back after a restart - review
threads, the `/spinner` override, the
per-account usage cache, cached model pricing, and the `/usage` view's
per-file token summaries.

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
