# Reference: errors, theme, glyphs

## Error states

forge has no dedicated full-screen error views. Error and failure UI arrives as system-message notices in the chat scrollback (see [Chat](./chat.md)) at one of three severities; the scrollback is the only error surface, and on a fatal connection error the input area locks with a hint rather than being replaced.

**Connection failed** - the CLI cannot start or the subprocess dies. The body is the failure line plus `Input disabled after an error. Press Ctrl+Q to quit and try again.`; the session id, account and MCP state reset, pending submits clear, and the input goes read-only.

<div class="term">

  <pre class="indent">
  <span class="error">Connection failed: claude binary not found at /usr/local/bin/claude</span>

  <span class="error">Input disabled after an error. Press Ctrl+Q to quit and try again.</span></pre>

</div>

**Settings parse error** - when `~/.claude/settings.json` (or a project settings file) cannot parse: a Warning notice naming the file path and the parser's location hint, deduplicated per file per parse attempt.

<div class="term">

  <pre class="indent">
  <span class="warning">Failed to parse ~/.claude/settings.json: expected `,` at line 42 column 5. Falling back to defaults.</span></pre>

</div>

**Rate limit notice** - on `AllowedWarning` (Warning) or `Rejected` (Error). Three branches: org-level disabled extra usage points at `/extra-usage`, `/model`, or waiting for the window; near-threshold-without-overage reads "Near rate-limit threshold. Resets in 4h 23m at 14:30 UTC."; the general case reads "{Approaching | Rate limit reached}, you've used N% of your `<type>` rate limit." with the overage status and reset time. The same message tints the rate-limit chip on the assistant's reply.

**Tool-use error** - a failed tool call renders the red `✗` icon with its error body in red; for Bash only the first non-empty stderr line shows. Internal failures (timeouts, panics, SDK protocol violations) render a bold red "Internal Agent SDK error" header over a red summary line instead.

<div class="term">

  <pre class="indent">
  <span class="error">✗</span> <span class="bold">⬚</span> crates/forge-tui/src/missing.rs
  <span class="dim">│  </span><span class="error">File not found: crates/forge-tui/src/missing.rs</span>
  <span class="dim">└─ </span><span class="error">error path: open() returned ENOENT</span>

  <span class="error">✗</span> <span class="bold">▶</span> cargo build --release
  <span class="dim">└─ </span><span class="error">error: failed to compile due to 3 errors</span></pre>

</div>

<div class="term">

  <pre class="indent">
  <span class="error">✗</span> <span class="bold">⬚</span> crates/forge-tui/src/something.rs
  <span class="dim">│  </span><span class="error bold">Internal Agent SDK error</span>
  <span class="dim">└─ </span><span class="error">stream closed at offset 12483 mid-frame</span></pre>

</div>

**Slash-command error** - a failed slash command (bad arguments, unknown command) is an Error notice in the same shape as connection-failed, without the input lock.

## Theme tokens

Hardcoded in the theme module - no light mode, no custom themes. Anything not listed renders with the terminal's default foreground and background.

| Token | Value |
|---|---|
| RUST_ORANGE | `Rgb(244, 118, 0)` |
| DIM | dark gray |
| USER_MSG_BG | `Rgb(40, 44, 52)` |
| STATUS_ERROR | red |
| STATUS_WARNING | yellow |
| SLASH_COMMAND | light magenta |
| SUBAGENT_TOKEN | light blue |

## Glyphs in use

Every character used as chrome - borders, icons, status, separators. Scope is current state only: anything new lands here in the same change as the code.

<details>
<summary>The glyph inventory</summary>

| Glyph | Where | Meaning |
|---|---|---|
| `╭ ╮ ╰ ╯` `─` `│` `├` `└` | panes, separators, borders, tool body, file trees | corners, lines, tree connectors (`└─` marks the last child or final body row) |
| `⬚` `▣` | Read; Write / Edit family | open square: read-only; filled square: mutation |
| `⌕` `▶` | Glob / Grep / LS; Bash | magnifier; execute |
| `◇` `◆` | Task / Agent; Workflow, WORKFLOWS header, projects-pane completed-unseen | hollow diamond: delegated subagent; filled diamond: script flow, or a completed turn on an inactive tab (green) |
| `⊕` `⊙` `⇄` | WebFetch / WebSearch; plan-mode and Config; Move and worktree tools | fetch from outside; meta operation; directory and worktree transitions |
| `◉` `◍` | TaskOutput / Monitor; TaskStop | fisheye observes; the vertical-fill circle terminates |
| `⏲` `*` | ScheduleWakeup; CronCreate / CronDelete / CronList | timer clock; the cron family |
| `◈` | Gotify icon; MCP-server line in a group summary | the shared Gotify diamond; marks an external MCP-server call |
| `⚠` | degraded states, AuthRequired, GOTIFY stream down, Bailed accounts | warning: noticeable but not broken - yellow; one deliberate split: a Bailed **auth** failure renders it red (repair needs an env edit plus restart), a transient failure yellow (the pollers heal it) |
| `✦` `⌖` | Skill / Advisor; ToolSearch | meta capability; tool search |
| `▲` `⇨` `⚙` | PushNotification; RemoteTrigger; LSP | outbound signal; remote trigger; tooling integration |
| `○` | fallback tool icon; pending todo; unfocused permission option; sleeping project row | open circle, reused |
| `✓` `✗` | completed / Allow; failed / Reject | completed and failed |
| `➤` | input prompt; SendMessage | prompt char |
| `▁▂▃▄▅▆▇█` | dictate level meter | the composer's 26-cell block ramp |
| `●` `◌` | recording / connected / in-force; transcribing / pending | filled and dotted circles |
| `·` `•` | permission option separator, sleeping project row; plan-approval actions, `/account` current marker | middle dot; bullet |
| `▸` | highlight cursor; selection indicator; in-progress todo | small right triangle |
| `△` `✕` | projects-pane and NEEDS ATTENTION; failed turn or worker; overlay close | waiting on the user (yellow); failed (red); the overlay `✕` dismisses |
| `▤` | narrow-tier Projects top-bar icon | toggles the Projects overlay |
| `💬` `✎` `↳` | diff comment cards | the card and rail badge, your editable turns, the reply line |
| `?` `[ ]` | question header and help toggle; mode badge and checkbox | rust orange in the question header; ASCII brackets |
| `▓ ░` | projects-pane usage bars | filled cells color by position (four zones); empty cells dim |
| `⎇` | Inspector GIT branch marker | dim on default, rust orange on a feature branch, yellow `HEAD` when detached |
| `⠋ ⠙ ⠹ ...` | spinner frames; running project rows | braille spinner |
| `>` | trust prompt and MCP list selection | selected marker |

</details>
