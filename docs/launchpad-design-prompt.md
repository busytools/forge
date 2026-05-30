# Launchpad view - Claude Design prompt

> **For:** an LLM-driven design pass (Claude Design, claude.ai chat,
> or any frontier-model UI design tool). Self-contained - you don't
> need other context to act on this. Output: HTML/CSS mockup matching
> the style of `docs/forge-map.html` (sibling file, attach it as a
> reference if the tool supports attachments).

## What forge is

forge is a terminal UI ("TUI") built in Rust on top of `ratatui`,
wrapping Anthropic's `claude` CLI. It runs as a personal-use
multi-project agent harness. The layout (current state, see
`forge-map.html` for full mockups):

```
┌──────────────────────┬──────────────────────────────┬───────────────────────┐
│  PROJECTS  (left)    │  Chat area / Welcome (main)  │  INSPECTOR  (right)   │
│  ─────────           │                              │  ─────────            │
│  Busytools           │   < forge welcome message >  │  GIT                  │
│   ├─ forge   [x]     │                              │    ⎇ branch-name      │
│   ├─ data-modules [x] │   user message ────►         │    +12 -5             │
│   └─ web-api       │                              │                       │
│                      │   < assistant streaming      │  TASKS · 2/5          │
│  Gateway             │     reply >                  │    ✓ done one         │
│   ├─ core-v1  [x]    │                              │    ⠋ in progress      │
│                      │   ─────────────              │    ○ pending          │
│  Personal            │   > Type a prompt ▌          │                       │
│   └─ companies       │                              │  PROCESSES            │
│                      │  ─────────────────────────   │    ⠋ cargo · 64 MB    │
│  Stargate            │  Profile  Stargate           │    └─ rustc · 100 MB  │
│   └─ stargate [x]    │  Mode     Auto               │                       │
│                      │  Model    Opus 1M            │                       │
│                      │  Effort   Max                │                       │
│                      │  Ctx ▓▓▓▓▓▓░░ 70%            │                       │
│                      │  5h  ▓▓░░░░░░ 25%            │                       │
│                      │  7d  ▓▓▓▓▓░░░ 60%            │                       │
└──────────────────────┴──────────────────────────────┴───────────────────────┘
```

- **PROJECTS pane (left)**: org-grouped tree from `~/.claude/forge.toml`.
  Live sessions show a filled `●` glyph (orange for the focused
  session, gray for background); idle catalog entries show hollow
  `○` + last-activity time. Live rows carry a 3-cell ` x ` close
  button on the right.
- **Chat area (main)**: where the conversation lives once a session
  is active.
- **INSPECTOR pane (right)**: GIT / TASKS / PROCESSES sections for
  the focused session.
- **Account/status panel** (left pane bottom): Profile / Mode / Model /
  Effort / context-usage + 5h/7d rate-limit bars.

The braille-spinner glyph `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` is reused across the chat
area, input editor, PROCESSES rows, Projects pane live rows, and
TASKS in-progress rows. It cycles at one frame per render tick.

## Why we need a launchpad

Today, forge boots into the chat view immediately - even before any
session has connected. The chat-area welcome message renders
against a still-spawning session and the bottom panel shows
placeholder dashes until OAuth/usage data lands. There's a real
bug where the first session in flight loses its wire messages (see
[busytools/forge#126](https://github.com/busytools/forge/issues/126))
which manifests as "I opened forge, it shows the forge cwd in
inspector but the session is empty + can't be reselected."

The launchpad is the answer: when no project is currently focused
(or when the user explicitly returns to the launchpad mid-session),
the chat area gets replaced with a landing screen showing the
forge logo + project picker. Picking a project promotes its
session to focused and transitions back to the chat view.

## What to design

A full-screen launchpad view for the **chat area only** (the left
PROJECTS pane stays visible; the right INSPECTOR pane CLOSES while
launchpad is up). When the chat area becomes the launchpad, it
holds:

### 1. Identity block (centered, top)

- **forge logo** - large ASCII-art or unicode art. Existing welcome
  uses a small forge logo; the launchpad version should be 3-5x
  bigger and the visual anchor of the page. ASCII art preferred
  (renders identically across terminals) but unicode block-art is
  acceptable.
- **forge version** (e.g. "v0.15.1 · 7d88141") under the logo, in
  dim text.
- **claude CLI version** (e.g. "claude 2.1.133") under that.
- **"↑ v0.15.2 available"** style indicator when an update exists,
  in `--rust-orange` (existing color from the dim panel).

### 2. Project picker (centered, below identity)

Same org-grouped tree from the left PROJECTS pane, but bigger and
formatted as the primary picker. Each project row:

- Status glyph (left): pending = `○` dim, loading = animated
  spinner (see "Loader options" below), connected/running =
  filled `●` colored by lifecycle, failed = `✗` red.
- Project name (middle).
- Account hint (right, dim): "(Personal)" or "(Gateway)" - which
  account this project's session will spawn against.
- Last activity time (far right, dim): "1w", "2d", "now".

Org headers in DIM bold, projects nested with `├─` / `└─`
connectors matching the current left pane's tree style.

Keyboard nav:

- `↑` / `↓` move selection through the project rows.
- `Enter` pick the highlighted project → close launchpad, promote
  that session to focused.
- `Esc` no-op (the launchpad is the floor; nothing to dismiss to).
- Vim-style `j` / `k` as an alternative is fine but not required.

### 3. Footer hint (centered, bottom of chat area)

Minimal dim text:
```
 ↑↓  navigate     enter  open project     ?  help     ctrl+q  quit
```

### 4. Spawn states to design for

The launchpad should render usefully in these states:

- **Cold boot**: no projects have spawned yet, no usage data, no
  catalog. Show "Loading projects..." with a spinner above the
  project list, and render each project row in a "pending" state.
- **Partial load**: some projects' sessions are connected, others
  still spawning. Each row's status glyph reflects its individual
  lifecycle.
- **Steady state**: all projects loaded (some live, some idle from
  disk catalog). Picker is fully usable.
- **Error state per project**: a single project's spawn failed  - 
  show `✗` glyph + dim error text under the row, picker still
  usable for the other projects.

## Loader options - make them configurable

Currently forge uses a single braille spinner everywhere. For the
launchpad we want a distinct loader (separate from the chat-area
braille spinner) so the visual language is clear: launchpad
spinner ≠ in-conversation spinner.

**Design 5+ alternatives** and recommend a default. Then propose a
`~/.claude/forge.toml` schema for letting the user pick:

```toml
[ui]
# Spinner style for the launchpad's "loading projects" indicator
# and per-project loading glyph. Doesn't affect the in-chat
# spinners - those stay braille for consistency.
launchpad_spinner = "phase_of_moon"  # or "quadrant" / "arc" / ...
```

Candidate spinners I'm seeding the design with - feel free to add
your own, drop any of these, or propose new ones:

| Key | Frames | Vibe |
|---|---|---|
| `braille` | `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` | Default everywhere else; familiar |
| `phase_of_moon` | `◐◓◑◒` | Calm rotation, distinct from braille |
| `quadrant` | `▘▝▗▖` | Crisp, geometric |
| `quarter_arc` | `◜◝◞◟` | Minimal, hand-drawn feel |
| `pulse` | `○◔◑◕●◕◑◔○` | Breathing fill, "alive" |
| `bouncing_dot` | `⠁⠂⠄⠂` | Quiet, single-cell braille |
| `forge_dot` | `●` (alpha tween) | Branded - solid rust-orange dot fading in/out via terminal color intensity |
| `classic_ascii` | `\|/-\` | Old-school, pure ASCII (renders even in dumb terminals) |
| `dots_wave` | `⠁⠃⠇⡇⡏⡟⡿⠿` | Braille fill - directional |

For each candidate, the mockup should show:
- One frame inline in the launchpad's "Loading projects..." row.
- A 4-5 frame strip below it so the reviewer can see the
  animation step.
- A per-project row using the same spinner mid-spawn.

The mockup should make picking a favorite easy.

## What to OUTPUT

A single self-contained HTML file styled to match `forge-map.html`'s
visual conventions:

- Reuse `forge-map.html`'s CSS variables (`--bg`, `--rust-orange`,
  `--dim`, `--muted`, `--user-msg-bg`, etc.).
- Mono font (`Berkeley Mono` / `JetBrains Mono` / `SF Mono` /
  `Menlo` fallback) for TUI mockups; sans-serif for prose.
- Section structure: header → "When the launchpad shows" → "Cold
  boot mockup" → "Partial load mockup" → "Steady state mockup" →
  "Spinner alternatives" → "Config schema" → "Keyboard map".
- Each TUI mockup is an ASCII art block inside a `<pre>` with a
  panel background, mimicking how forge-map.html renders its
  current TUI states.
- Use the `--rust-orange` for focused-state glyphs, `--dim` for
  inactive text, matching the existing forge palette exactly.

## Reference materials

- `docs/forge-map.html` - visual truth for all CURRENT forge-tui
  surfaces. Read it FIRST. The launchpad will become a sibling
  section in this file once the design is approved.
- The "Why this is a separate session" + open design questions in
  the project-memory brief at
  `~/.claude-profile3/projects/-Users-dev-Projects-forge/memory/brief_launchpad_view.md`
  for additional context (questions about whether Inspector pane
  reopens automatically, slash command to re-show, etc.).
- forge.toml schema for orgs/projects: `crates/forge-workspace/src/config.rs`.
- Current spinner constant + theme colors: `crates/forge-tui/src/ui/projects_pane.rs`
  (`SPINNER_FRAMES`) and `crates/forge-tui/src/ui/theme.rs`.

## Open design questions to answer in the mockup

1. **Inspector pane during launchpad** - auto-close (free up the
   chat width for a bigger logo), or stay open showing "GIT" /
   "TASKS" / "PROCESSES" placeholders? Pick one and justify.
2. **Slash command to re-show** - `/launchpad`? `/home`?
   `/welcome`? Pick one.
3. **What happens to background `auto_start` projects while the
   launchpad is up?** Should they still spawn (so picking one is
   instant), or wait until the user picks?
4. **Persisted last-picked project** - should forge remember the
   last project the user picked from the launchpad and reopen
   directly into that session next launch (bypassing the
   launchpad)? Or always show the launchpad until the user
   explicitly sets `focus = true` in forge.toml?

Answer these in a short "Design decisions" section at the bottom
of the HTML output.

---

That's the brief. Read `forge-map.html` for style; output a single
HTML file matching its conventions. Skip explanations in the
output - show the design.
