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

| Key                 | Type             | Required | Notes                                                               |
| ------------------- | ---------------- | -------- | ------------------------------------------------------------------- |
| `name`              | string           | yes      | Must be unique across orgs.                                         |
| `accounts`          | array of strings | yes      | Each entry must match an `[[accounts]]` `display_name`.             |
| `fallback_accounts` | array of strings | no       | Accounts the walk reaches after the pinned ones. Absent means none. |
| `projects`          | array of tables  | yes      | Written as `[[orgs.projects]]`. An org with none fails the load.    |

`accounts` is the account subset every project in this org may spawn
under. Rules enforced at load:

- An empty list (`accounts = []`) fails with `has an empty
accounts = [] list`.
- A name that matches no `[[accounts]]` entry fails, and the error
  lists the valid names.

`fallback_accounts` is walked after the org's `accounts`: a fallback
declaring the project's model outranks a saturated or bailed primary,
and the primary is taken again once it heals. The names also carry as
the fallback pin on the `accounts` subject. The list is validated at load
exactly like `accounts`: a name matching no `[[accounts]]` entry fails
the boot, naming the account and the valid names. An account may appear
in both lists; it is then a primary only.

`accounts` and `fallback_accounts` must also agree on any env that
changes the _shape_ of a request, not only where it is sent. A child's
env is frozen when its session spawns and the gateway never respawns
it, so after a rotation the next account is handed a request shaped for
the account the session started on. The live example is
`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS`: OpenRouter sets it and Zai
leaves it unset, both declare `glm-5.3-flash`, and OpenRouter rejects
the beta shape the flag suppresses. A project declaring `glm-5.3-flash`
in an org holding both would spawn on Zai and then fail every rotation
onto OpenRouter, with a restart the only way out.

## `[[orgs.projects]]`

| Key          | Type   | Required | Default | Notes                                                                                            |
| ------------ | ------ | -------- | ------- | ------------------------------------------------------------------------------------------------ |
| `name`       | string | yes      |         | Must be unique across _all_ orgs, not just within one.                                           |
| `path`       | string | yes      |         | A leading `~/` is expanded to the home directory. The un-expanded string is kept for display.    |
| `auto_start` | bool   | no       | `false` | When true, the project's lead session spawns at forge launch. Any number of projects may set it. |

Project names are the argument `forge <PROJECT>` takes, and the key
per-project settings hang off.

## `[[accounts]]`

An array of tables. At least one is required, or the load fails with
`no [[accounts]] entries in forge.toml`.

| Key             | Type            | Required | Default | Notes                                                                                                                                                                                                                                                                                                                                                                                                                    |
| --------------- | --------------- | -------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `display_name`  | string          | yes      |         | Must be unique. This is the name orgs reference.                                                                                                                                                                                                                                                                                                                                                                         |
| `provider`      | string          | yes      |         | One of `"anthropic"`, `"codex"`, `"openrouter"`, `"zai"`. Decides how the account is probed and how its usage reads.                                                                                                                                                                                                                                                                                                     |
| `token`         | string          | yes      |         | The credential the gateway forwards, mapped onto the provider's own variable at spawn. Trimmed once at load.                                                                                                                                                                                                                                                                                                             |
| `base_url`      | string          | no       |         | The upstream base for base-url providers (`"codex"`, `"openrouter"`, `"zai"`); required for them and validated per provider. An `"anthropic"` account may omit it - the gateway constant is its upstream.                                                                                                                                                                                                                |
| `models`        | list of strings | yes      |         | The canonical model names the account serves. Selection and the route's model gate match against this list; an empty list fails the load (`AccountModelsRequired`).                                                                                                                                                                                                                                                      |
| `model_slugs`   | table           | no       | `{}`    | Canonical name -> upstream spelling, only where they differ. Every slug key must be in `models`, or the load fails (`AccountSlugUndeclared`).                                                                                                                                                                                                                                                                            |
| `model_aliases` | table           | no       | `{}`    | Written as `[accounts.model_aliases]`. Canonical name -> the other names a request may arrive under for that model. Every key must be in `models`, or the load fails (`AccountAliasUndeclared`); an empty list fails (`AccountAliasEmpty`).                                                                                                                                                                              |
| `env`           | table           | no       | `{}`    | Written as `[accounts.env]`. Provider-behaviour extras only - timeouts, context caps, fallback switches. Gateway keys (`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`) declared here or in the global `[env]` layer are dropped at load and warned about: the flat `base_url` and `token` keys own them, and forge reads the account's endpoint and credential from those. |

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

## `[models."<name>"]`

Optional per-model tables, keyed by the exact spelling a project's
`model` pins - that is the name the read matches on, not what accounts
declare - so one entry serves every project pinned to that model. Absent
means no parameters are stamped and the `claude` CLI's own resolution
stands. A table naming a model no project pins loads, but warns at boot:
the entry is read by the projects' pins, and no pin carries that name.

| Key                   | Type    | Required | Default | Notes                                                                     |
| --------------------- | ------- | -------- | ------- | ------------------------------------------------------------------------- |
| `auto_compact_window` | integer | no       | unset   | The window, in tokens, the CLI's auto-compaction triggers against.        |

```toml
[models."deepseek-v4.1-flash"]
auto_compact_window = 983040
```

`auto_compact_window` is stamped onto the spawned child as
`CLAUDE_CODE_AUTO_COMPACT_WINDOW`. Set it below the model's real context,
leaving the compact query room to fit: compaction fires near a window's
top, and the compact request itself needs headroom beyond that for the
completion budget and the prompt it carries - a window allowed to reach
the model's context can fire a compact request the context cannot hold,
and the `claude` CLI exits with the session, which is what this key
keeps out of reach. The value is yours to pick because forge does not
know a model's real context: leaving a sixteenth of the context as
headroom (a 983040 window for a 1,048,576-token model) is measured to fit
with room over. An env layer that names
`CLAUDE_CODE_AUTO_COMPACT_WINDOW` itself replaces the table's value.

A value that is not a positive integer fails the load:
`models "name" in forge.toml ... sets auto_compact_window = 0, which is
not usable; it must name a positive window in tokens`. Unknown keys
inside a `[models."<name>"]` table are rejected, so a mistyped key fails
the load instead of quietly applying nothing.

## Per-project keys (inside `[[orgs.projects]]`)

The per-project settings live directly on each `[[orgs.projects]]`
entry, beside `name`, `path` and `auto_start`. The entry rejects
unknown fields, so a mistyped key fails the load loudly instead of
quietly applying nothing.

| Key               | Type    | Default | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ----------------- | ------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `env`             | table   | `{}`    | Written as `[orgs.projects.env]` directly under the entry, or inline as `env = { ... }`. The block form attaches to the MOST RECENT `[[orgs.projects]]` header - place it directly under its own entry, before the next header, or it lands on a different project.                                                                                                                                                                                                                                                                                                            |
| `env_file`        | string  | none    | Path to a `KEY=value` file whose entries join this project's env.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `max_workers`     | integer | `2`     | Cap on this project's concurrently live dynamic workers. The count is per project: workers live in other projects neither consume this project's budget nor raise its cap. A spawn over the cap errors instead of queuing; despawning a worker frees its slot. Workers restored by the boot or lead-reconnect respawn of persisted rows are exempt, but still count toward the cap once live. `0` disables dynamic spawns for the project.                                                                                                                                     |
| `permission_mode` | string  | `auto`  | Stamps the CLI's permission mode onto every session this project spawns, overriding the session default. Absent means `auto`, not the session default, so a project's sessions run one mode however its org's accounts rotate.                                                                                                                                                                                                                                                                                                                                                 |
| `model`           | string  |         | The project's model. Fills the CLI's model slots at spawn (`ANTHROPIC_DEFAULT_HAIKU_MODEL`, `_OPUS_MODEL`, `_SONNET_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL` all carry it) and is the session default a `/model` change overrides. Must be declared by at least one account in the org, or the load fails - a model no account serves would otherwise stamp every slot and 503 each session's first request. Required: the walk that picks a session's account matches on the model an account serves, so a project without one is refused at spawn. |
| `vault_roots`     | array   | `[]`    | Extra roots the document read accepts beyond the session's own tree, so a project's documents can live outside its repo (a vault, a shared spec directory). Each entry is an absolute path, `~` or a `~/` path; a relative one fails the load. |

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

### `vault_roots`

```toml
[[orgs.projects]]
name = "companies"
path = "~/Projects/companies"
vault_roots = ["~/Madness"]
```

Extra roots the document read accepts beyond the session's own tree, so
a project's documents can live outside its repo. The `documents__hand_off`
tool, the viewer's document read and the tree review placement all
consult the seat's project's roots; the file-index walk does not, because
it lists the session's tree.

The check canonicalizes both sides and resolves symlinks before
comparing, so a symlink whose target exists and sits out of every root
refuses like any other escape - a dangling one reports the document
missing, exactly as it does in-tree - and a path inside a root reads
exactly as an in-tree path does. A path outside the tree and outside
every root keeps the refusal it always had: `the path resolves outside
the session's tree: <path>`.

Each entry is an absolute path, `~` (your home directory) or a `~/`
path. Anything else fails the load naming the key and the entry:
`project 'companies' in forge.toml at ... sets vault_roots entry
'Madness', which is not absolute; every entry must be an absolute path,
~ or start with ~/`, because a relative root would resolve against
whatever directory the read happened in.

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

| Key                      | Type    | Default | Notes                                                                                                                                                                                                                                                                                                                                                                                                        |
| ------------------------ | ------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `port`                   | integer | `8787`  | The port the gateway's listener binds on `127.0.0.1`. There is no fallback to an OS-assigned port: if the port is taken, the gateway fails to start and preflight stays shut, because every session's base URL names this port and a silent drift would point children at an address nothing serves. `0` fails the load outright (`GatewayPortInvalid`) for the same reason - it reads as "pick one for me". |
| `streak_count`           | integer | `5`     | How many consecutive 429s from one account fire a rotation. `0` fails the load outright (`GatewayRotationInvalid`) - it would silently disable the streak.                                                                                                                                                                                                                                                   |
| `streak_window_secs`     | integer | `60`    | The window the 429 streak is counted over, in seconds. `0` fails the load outright (`GatewayRotationInvalid`).                                                                                                                                                                                                                                                                                               |
| `no_reset_cooldown_secs` | integer | `60`    | The cooldown applied when neither the failing response nor the account's own usage probe reports a reset time, in seconds. `0` fails the load outright (`GatewayRotationInvalid`).                                                                                                                                                                                                                           |

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
_project_ layer instead desynchronises forge's own accounting, because
the usage probe, plan detection and the account pool all read the
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

## Notifications

Notifications are the client's own: a windowed client raises them
through its host (a system banner, a dock badge, whatever the platform
does), and there is no `forge.toml` key for any of it. The server
raises nothing itself - it has no window to put a banner on.

There is no `[ui]` section either: the spinner and repaint keys the
terminal drew with retired with the terminal crate, and no key replaced
them. A `forge.toml` still carrying the
section loads and warns rather than failing, so the keys can be deleted
at leisure; see the ghost list under "Unknown keys".

## `[dictate]`

Optional. Absent means dictation is off, which is also what an explicit
`enabled = false` means.

| Key                   | Type    | Default            | Notes                                                                                                                                                                                                                                           |
| --------------------- | ------- | ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enabled`             | boolean | `false`            | Off unless asked for. Turning it on costs a 3.07 GB model download the first time and holds about 1.8 GB of resident memory for the run.                                                                                                        |
| `models_dir`          | string  | platform cache dir | Where the model files live. `~` is expanded.                                                                                                                                                                                                    |
| `transcribe_model`    | string  | none               | The model the transcribing role runs, by the runtime's own variant name. Set, it wins over every runtime pick and refuses an activation for that role; absent leaves the role to the last pick made on this machine, else the compiled default. |
| `cleanup_model`       | string  | none               | The same for the cleanup role. Inert while `normalizer = false`.                                                                                                                                                                                |
| `language`            | string  | autodetect         | Spoken language hint.                                                                                                                                                                                                                           |
| `normalizer`          | boolean | `true`             | Rewrite recognition output into clean text. Off halves the download and skips a pass per utterance.                                                                                                                                             |
| `max_capture_minutes` | integer | `30`               | Upper bound on one recording. A capture reserves memory eagerly, about 110 MiB at the default.                                                                                                                                                  |
| `bind`                | string  | `right_cmd`        | The push-to-talk key: `right_cmd`, `left_cmd` or `off`. On Linux and Windows the cmd equivalent is the right/left Control key.                                                                                                                  |
| `mode`                | string  | `auto`             | How press/release maps onto recording: `auto` infers from timing (a quick tap toggles, a hold transcribes on release), `toggle` starts on a press and stops on the next press, `hold` records while held and always transcribes on release.     |
| `styling`             | string  | `semi_formal`      | The register a take's cleanup rewrites into: `casual`, `semi_casual`, `semi_formal` or `formal`. A client reads this as the value its panel starts on and resets to.                                                                            |
| `structure`           | string  | `prose`            | Whether the cleanup may return a bulleted list: `prose` or `lists`.                                                                                                                                                                             |
| `context`             | string  | `general`          | What the words are being written for: `general` or `email`. `email` may return multi-line text.                                                                                                                                                 |

An unrecognised key here fails the load rather than being ignored: a
mistyped `models_dir` would otherwise fetch three gigabytes to the
wrong volume with nothing said about it. `device` is the one exception -
a declared ghost: the microphone is always the reader's, so the key is
warned about and ignored rather than obeyed, and a config still setting
it keeps loading.

The models live where forge runs; the microphone is wherever the reader
is: a client captures on its own machine and streams the audio to forge,
where the pipeline recognises it. `styling`, `structure` and `context`
are defaults rather than state: they reach every client in the greeting,
and a client that captures holds the values in force itself, per seat, so
its panel's reset row returns to these.

With `enabled = true`, forge fetches, verifies and loads the models on
the preflight screen before forge hands over. A first run
downloads 3.07 GB, resumable and SHA-256 verified; a later run re-hashes
only a file that has changed since its digest was recorded, about a
second for the pair, and checks an unchanged one with a stat. Then the
weights load. Pressing `esc` during a download keeps what has landed
and quits.

**A role's model is resolved fresh each boot, in this order**: the
`transcribe_model` / `cleanup_model` key when set, else the last runtime
pick made on this machine (kept in the state store beside forge's other
machine-local state, not here), else the compiled default for this binary.
Setting a key never clears the pick, so removing the key returns the role
to the last runtime choice rather than to the compiled model.

**A key names a variant, not a path** (`granite-speech-5.0-470m-turboctc`):
this file syncs across machines and a path is per-machine. The variant
resolves through the machine's installed models first - a local file, no
network - and then through the runtime's catalogue, whose per-model doc
carries the download URL. A variant that resolves through neither fails the
boot, naming the key and the variant:

    [dictate] transcribe_model names granite-470m, which is neither an installed model nor a catalogue entry

A variant that resolves but is not on disk yet is fetched by the boot
preflight, the way the compiled defaults already are, and records as
installed.

**A key pins the role**: the runtime cannot change it, and the models page
refuses an activation for it, naming the key:

    [dictate] transcribe_model pins this model in forge.toml; remove the key to change it here

Downloads and benchmarks stay allowed on a pinned role - a pinned role can
still pull candidates down and bench them; only the running model cannot
change without editing this file.

The compiled model files themselves are not configurable. Each carries a
URL, a byte length and a digest, and a hand-edited one is a file nothing
can verify. A model named by the keys above, or downloaded from the models
page, is checked against the feed's own byte length and the engine's own
load instead: those files publish no digest, and nothing forge prints calls
one verified.

## `[gotify]`

Optional. Absent means the Gotify integration stays dormant.

| Key            | Type   | Required | Notes                                                    |
| -------------- | ------ | -------- | -------------------------------------------------------- |
| `url`          | string | yes      | Gotify server URL.                                       |
| `client_token` | string | yes      | Token for the receive stream and the application lookup. |

Both are mandatory once the section is present; neither has a default.

## `[systemone]`

Optional. Absent or `enabled = false` means the decision tools
(`systemone__ask_noul`, `systemone__ask_choice`, `systemone__ask_score`)
are not injected into any session.

| Key          | Type    | Required | Notes                                                                                                    |
| ------------ | ------- | -------- | -------------------------------------------------------------------------------------------------------- |
| `enabled`    | boolean | no       | Defaults to `true`. `false` keeps the section (and the key) without injecting the tools.                 |
| `base_url`   | string  | yes      | The endpoint's base. forge appends `/v1/systemone`, or `/systemone` when the base already ends in `/v1`. |
| `api_key`    | string  | no       | Sent as a bearer token when set; omit it for keyless local hosts such as Ollama.                         |
| `model`      | string  | yes      | The model every call asks, e.g. `jev-latest`.                                                            |
| `timeout_ms` | integer | no       | Per-request timeout in milliseconds; defaults to 30000.                                                  |

`base_url` and `model` are mandatory once the section is enabled; a
missing one refuses the load naming the key - the message reads
``[systemone] `base_url` is required when `[systemone]` is enabled``
(or the same sentence for `model`) - and `timeout_ms = 0` refuses with
``[systemone] `timeout_ms` must be greater than zero``. A key the
endpoint itself rejects surfaces as a tool error naming `api_key`;
forge never substitutes another credential.

What the endpoint takes: up to 255 options in a choice question, 2 to 10
levels in a score question, and a whole request - state, instructions and
the question together - inside the live context, which OpenRouter lists
as 32000 tokens for these models. Upstream describes the shape as 32k of
state under a 64k request; the live 32000 is what to size against, with
the state kept well inside it. It serves about 100K tokens/s at 80
requests/s and bills $0.042 per million input tokens, rates upstream
notes can change without notice; the default 30s `timeout_ms` has ample
headroom.

## `[[slack]]`

Optional, and repeatable: one entry per Slack workspace. Absent or empty
means the Slack connector stays dormant.

| Key                | Type    | Default | Notes                                                                                                                                                                                                                       |
| ------------------ | ------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `workspace`        | string  | none    | Label for this workspace, distinct per entry. It addresses the workspace in `slack__list`.                                                                                                                                  |
| `token`            | string  | none    | User token, `xoxp-...`.                                                                                                                                                                                                     |
| `poll_seconds`     | integer | `5`     | Sweep interval for this workspace's conversations, in seconds.                                                                                                                                                              |
| `dm_poll_seconds`  | integer | `150`   | Sweep interval for the DM class, in seconds. It covers every DM in the workspace rather than a list, so it costs one history call per DM per pass whatever the subscription count, and it is swept on its own slower clock. |
| `thread_idle_days` | integer | `14`    | Drop a followed thread with nothing new for this many days. A thread that quiet is resolved in practice, whatever its parent's age.                                                                                         |
| `stale_after_hours` | integer | `24`    | Mark a delivered block's header with its newest message's age once that message is at or past this age. Zero turns the mark off; nothing is dropped.                                                                        |

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

| Key           | Type    | Default | Notes                                                                                                                                                              |
| ------------- | ------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
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

## The task board's chase

**Fixed behaviour, not configuration.** There is no `[tasks]` section:
`forge.toml` refuses one - and any key like `stale_hours` - as an unknown
key, and the chase has no switch: the sweep runs every 30 seconds,
always. What it asks, and on what clock, is a constant in the code:

- **The no-movement window is 4 hours.** A row with no update for that
  long draws the `no movement` mark; a ready row with no owner, and a
  dependency or resource wait, ask the lead every 30 minutes once they
  pass it. The estimate is what arms the chase for a row that has one;
  this is the fallback for rows that have none.
- **A row past its estimate asks its owner once**, and a restated
  estimate arms the nudge again; a wait that resolved asks its waiter
  once to resume it.
- **A row past 1.5x its estimate tells the lead every 5 minutes** while
  it stays over. The escalation is authority: rescope, reassign, take
  over, or restate the expectation.
- **A failed row, one whose owner is gone, an epic still owing its retro,
  and one ready to close each tell the lead every 30 minutes** while the
  state holds.
- **A worker holding no row is named when it drifts, again every 5
  minutes while it stays drifted**, and at once after it accounts and
  drifts again.
- **Each row waiting on the user, untouched for 30 minutes, gets one
  nudge of its own per wait episode** - one message per row, naming the
  row's ask, and never a repeat while that wait holds. A row the lead
  acts on (attested, withdrawn or parked by design) leaves the set, and a
  row that leaves waiting and comes back is a new episode with a new
  nudge. A tick that finds every seat asleep spends nothing: the nudge
  waits for a live lead.

## `[server]`

The socket: one WebSocket served from the process that already owns the
sessions, so a client costs a listener rather than a second scheduler.
On by default, so a restart leaves it serving.

| Key       | Type       | Default     | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| --------- | ---------- | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enabled` | boolean    | `true`      | Whether the server starts with forge. `false` is the opt-out, and the boot is identical either way - the socket simply does not bind.                                                                                                                                                                                                                                                                                                                                        |
| `port`    | integer    | `8790`      | The port the server binds on `bind`, and a client is pointed at it by hand. `0` fails the load outright (`ServerPortInvalid`), and so does the gateway's own port (`ServerPortTakenByGateway`) - two listeners cannot share one. Neither check runs while the socket is disabled: a stale port on a section that never binds cannot stop the boot.                                                                                        |
| `bind`    | IP address | `127.0.0.1` | The interface the server listens on. Loopback by default: reaching it from another machine means naming that machine's interface here, usually the WireGuard address. **forge is reached over loopback or a private network and is never exposed publicly.** That is the whole reason the socket carries no authentication - the private network is what the access control stands on, not a proxy to be added in front of a public bind. |
| `tool_record_cap_kb` | integer | `64` | How large a frame's `tool_use_result` may be in anything a client is served, in kilobytes. **A record over it is reduced first**, keeping only the keys a client draws (a mutation's patch and outside-edit mark, a question's answers and notes); when those alone are still over, it keeps the one key the row's drawing needs - a question's `answers` or an edit's `structuredPatch` - so a question whose typed notes pushed it over keeps its answer and draws as the card it was. Only when nothing drawable fits does it become an `omitted` marker. Every path to a client is bounded - a page, a `more`, the live stream and both repairs - while the transcript keeps the record whole and the tool-result block draws as always. Every record a served copy bounds is logged at DEBUG as `tool_record_capped`, naming the seat, the tool call, the record's original size, the keys it kept and its `disposition` (`reduced` or `omitted`), so a reader asking where a payload went can find the exact call. **The line is per capped copy, not per record**: a subscribe caps the snapshot and its newest page separately, and a live frame is capped once per reading client, so the same call can log more than once in one open - the call's own id is what ties the lines together. `0` serves the records whole - the setting for a reader whose changes carry patches beyond the default. A seat whose mailbox tool results ran to ~4.9 MB each served a 70 MB page that took the client ~6.7 s to settle; the cap is what makes a page's size follow its conversation rather than its tool payloads. |

The server starts no subsystem of its own - no cron scheduler, no
connector, no second gateway - so the cost of a client is the listener
and the messages. The socket carries the whole view surface: the home and
every session, a snapshot of each on subscribe and every update after it,
with a client's commands going back the other way. **No page is served
from here.** A client is its own program and draws everything itself; the
socket hands it data. A bind that fails is fatal on the headless binary:
with no view of its own it would serve nothing.

## `[client]`

Which of the sets forge ships a client draws with. Absent means every
name unset, which is the built-in.

| Key     | Type   | Default                                           | Notes                                                                                                                                                                                                                                                                     |
| ------- | ------ | ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `mark`  | string | unset, drawing panes                              | The mark a client draws, by name: `panes`, `klin`, `lanes`, `f_slab`, `split`, `spine`, `grid`, `clamp`, `strike`, `nest`, `chamfer`, `tally` or `stencil_f`. A fixed set rather than a file path, so every option is one forge has drawn.                                |
| `theme` | string | unset, drawing dark                               | The palette, by name. `dark` is the only one shipped so far; a second is its own pass, since every state colour needs a treatment that stays legible on the new background.                                                                                               |
| `font`  | string | unset, drawing Fira Code for prose and code alike | The typeface, by name. Fira Code is vendored, bundled with the client and licensed OFL 1.1; `system` is the opt-out to the stacks the OS already has. `--ui` and `--mono` come from here rather than from the stylesheet, so this is the only place a typeface is chosen. |
| `browser_idle_minutes` | integer | unset, meaning 20                      | The idle release: after this many minutes with no browser call, raise or lower, a named profile's creator is sent a heads-up over the socket's prompt channel. A live session gets the heads-up as a turn; a seat with no live session has nobody to tell, and its browser closes quietly instead. **The shared browser is never announced** - it closes quietly at the grace. **A hand-off waiting on the person always holds the profile** - it is neither warned about nor closed, and neither is a profile whose window is up. |
| `browser_grace_minutes` | integer | unset, meaning 10                     | How long the browser lives past its idle bound unless it is used - for a named profile, after its heads-up. The close is a real close: logins and the profile stay, and the next call brings the browser back on the page you last saw - the CEF host keeps one page target, so there are no Brave-era tabs to restore - but a filled form does not survive it. **The clocks live in the client's memory**, so a client restart grants every browser a fresh window. |

## `[documents]`

The ceiling on one document a session hands the reader through the
`documents__hand_off` tool, and on one file's content at a revision the
review page reads for a between-hunk expansion (`file_content`): both read
under this key. Absent means the default.

| Key            | Type    | Default | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| -------------- | ------- | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `max_size_mib` | integer | `10`    | The largest document the viewer opens, in MiB, between `1` and `190`. A read over it is refused naming the size, before any bytes are read. The review page's `file_content` read shares the ceiling and answers a flagged short read at it rather than a refusal, and caps its answer at 10,000 lines besides - which is what bounds a file of very short lines before the bytes do. `0` fails the load: a ceiling of nothing would refuse every document, which reads as a bug rather than a choice. A value past `190` fails the load too: a document crosses the socket base64-encoded, and a file read crosses as JSON lines whose escaping can cost about twice their bytes, both measured against the desktop client's frame cap of 256 MiB - a document at this ceiling stays inside it, and the file read's own 10,000-line bound is what keeps the ordinary case well inside it too. |

A row's motion is not configurable. The client animates its own: the
marks move by state - the working mark turns
in 900ms and the starting one in 1500ms, and that difference is how a
reader tells them apart.

An unset name is not an error and not a fallback: it means the built-in.
A name forge does not ship fails the load with the key and the value in
the message (`ClientNameUnknown`, `client mark = "anvilish" ... names
nothing forge ships; this key picks by name, never by path`), because a
setting that is quietly ignored reads as the key not working. Like the
port checks, none of them run while the socket is disabled.

`mark`, `theme`, `font` and the idle pair are a client's settings rather
than the server's, and the socket carries them to a client on connect, so a
client never reads `forge.toml` for itself and the file stays the one
source of truth. The server draws none of them: the socket serves no page.

## `[usage]`

How often the background poller refreshes account usage. Absent means
the default.

| Key            | Type    | Default | Notes                                                                                                                                                                                                                                                                                                             |
| -------------- | ------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `poll_seconds` | integer | `60`    | The refresh interval, in seconds. The session page draws its 5h/7d utilization from the cache this fills, and the gateway's account selection reads it too, so the interval is an upper bound on how stale a "which account has more headroom" decision can be. `0` fails the load: a zero interval would probe continuously. |

## Unknown keys

Every table rejects unknown fields, so a mistyped key fails the load
and names itself rather than parsing clean and meaning something else -
a misspelled `fallback_accounts` would otherwise read as "no
fallbacks". That covers the top level, `[[orgs]]`, `[[orgs.projects]]`,
`[[accounts]]`, `[models."<name>"]`, `[[slack]]`, `[gotify]`,
`[systemone]`, `[gateway]`, `[dictate]`, `[plugins]`, `[server]` and
`[client]`.

A key forge itself retired is a declared ghost rather than an unknown
key, so a stale `forge.toml` still boots and warns instead of failing:
`[workers]`, `[projects.<name>]`, `[selection]`, `[ui]` and `[dictate]
device`. In the tables above a stray key is a typo and is refused; a
ghost is the exception,
because its whole job is to accept what a live config still carries - so
what a retired section holds is unconstrained and only the section
itself warns.

**`[web]` is the one retired section that fails the load.** It is a
declared ghost too, but a hard stop rather than a warning
(`WebSectionRenamed`): `[web] was renamed ... its listener keys
(enabled, port, bind) are [server]'s and its drawing keys (mark, theme,
font) are [client]'s`. There is no alias for the old name, and an ignored
`bind` would quietly reset a server that was exposed on a network
interface to loopback.

`[ui]` is a ghost like the rest because dropping it as dead weight
would refuse the boot of every config still carrying one - on a machine
nobody is looking at, with a synced file. It holds nothing the server
reads, so the section is ignored and warned about rather than obeyed.

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
dm_poll_seconds = 150
thread_idle_days = 14
stale_after_hours = 24

[plugins]
auto_update = true

[server]
enabled = true
port = 8790
bind = "127.0.0.1"

[client]
mark = "panes"
theme = "dark"

[documents]
max_size_mib = 10
```

## What forge does at startup

`forge <PROJECT>` opens the named project. The name must match a
project's `name` exactly.

**Preflight runs first on every route.** It resolves every
`[[accounts]]` entry and, when `[dictate]` is on, fetches and loads the
dictation models, then hands over to wherever you were headed: the
project picker for `forge`, straight into that project's chat for
`forge <PROJECT>`. It is shown once per run.

When `[dictate]` is on, boot also reads the cached model catalogue and
refreshes it in the background - fetching the feed the dictation-models
page reads - when the cache is older than a day or absent. That refresh
never holds the boot path: it lands as an update on the page's own
subscription, and a `forge.toml` with dictation off fetches nothing.

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

The socket comes up on `[server] bind:port` during the same boot, unless
`[server] enabled` is false.

## Config versus state

`forge.toml` is the config half. It is read-only from forge's point of
view, which makes it safe to sync between machines.

Everything mutable lives in a single embedded redb database at
`<app-support>/db.redb`: durable crons, Gotify subscriptions, Slack
subscriptions and the sweep watermarks beside them, the session
identity per project and label - the lead's and every worker's, which is
what brings a worker back after a restart - review
threads, the
per-account usage cache, cached model pricing, and the usage scan's
per-file token summaries.

Dictation keeps four things outside the database, all machine-local
and never synced: each take's audio and transcripts, as plain files
under `<app-support>/dictate-diagnostics/` - voice recordings outside
the database - the read-aloud set the models page records under
`<app-support>/dictate-read-aloud/`, which is the one corpus a bench can
score on words that are known, the record of verified model digests under
`<app-support>/dictate-digests/`, which is what lets a boot over
unchanged models skip re-hashing them, and the last fetched model
catalogue under `<app-support>/dictate-catalogue/`, which is what lets
the models page read the feed when the network is gone.

`<app-support>` is `~/Library/Application Support/forge` on macOS
and `$XDG_DATA_HOME/forge` on Linux.

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
