# Workspace

forge ships as a 5-crate runtime workspace. Only `forge-tui` renders to the terminal; the other four layer underneath through strict acyclic dependencies. `forge-test-harness` is the wire-conformance harness - not in the runtime path.

| Crate | Role | depends on |
|---|---|---|
| forge-primitives | Wire-shape types shared across forge-* crates. Message envelopes, content blocks, hook/permission/option/subagent data, render-side views, channel commands, IDs. No logic, no I/O, no async. | nothing forge-shaped |
| forge-sdk | Wraps the `claude` CLI subprocess. Owns the stream-json codec, transport, control dispatch, in-process MCP host, callback registries, and Options builder. | forge-primitives |
| forge-agent | Drives one forge-sdk Client behind a channel-based Agent / AgentHandle API. Owns userdata (settings, trust, sessions catalog, memory, plugins), cloud (oauth/usage/account), env (git context), translate, and tooling. | forge-primitives, forge-sdk |
| forge-workspace | Multi-session orchestrator and TUI-facing facade. Per-session `DomainSession` holds operational state; per-session `SessionTask` actors pump `AgentHandle::take_events()` into `SessionUpdate`s and route `Command`s back. The single point of contact between TUI and the agent layer. | forge-primitives, forge-agent |
| forge-tui | Native ratatui terminal interface. Pure view layer; consumes `SessionUpdate` and dispatches `Command` via `forge-workspace`. Holds per-session `UiSession` for presentation state (messages, viewport, input editor, hover hints). **The subject of this map.** | forge-primitives, forge-workspace |
| forge-test-harness | Wire-conformance harness. `sdk_wire` scope (forge-sdk ↔ claude CLI). Replay-based offline tests + opt-in live capture. Not in the runtime path. | forge-sdk (transitively gets forge-primitives) |

# Layout

forge-tui's main frame is a strict vertical stack composed by `layout::compute`, flanked by inline side panes at the wider tiers. **This stack only renders in `ActiveView::Chat`.** When the user opens `/config`, forge swaps the entire frame to `ActiveView::Config` - chat / input / side panes all disappear. At terminal widths ≥160 cols the chat frame is flanked by a 32ch [Projects pane](./projects-pane.md) on the left (carrying the account / mode / model / usage / location panel at its bottom) and a 40ch [Inspector pane](./inspector.md) on the right (carrying the live `TASKS` list from `TodoWrite`); between 120-159 cols they shrink to 24ch and 30ch respectively, with truncation; below 120 cols a single-row top bar replaces both panes - tapping the `▤` icon (or <kbd>Cmd+Left</kbd>; <kbd>Ctrl+Left</kbd> off macOS) opens the Projects overlay, tapping `▦` (or <kbd>Cmd+Right</kbd>; <kbd>Ctrl+Right</kbd> off macOS) opens the Inspector overlay.

In `ActiveView::Chat` the central stack is:

1. `body` - chat scrollback (Min 3 rows, takes the flex)
2. `input` - the bordered input box (1 interior row, grows up to `MAX_INPUT_HEIGHT = 50` as the user types; its own thick top/bottom edges are the dividers - there are no separator rows)
3. `help` - help overlay (0 when inactive, else `HELP_PANEL_HEIGHT = 14`)

The mode / model / cwd / branch / usage **status used to sit as 2 rows below the input**; that row is gone. Mode / Model / Effort / usage moved to the [Projects pane](./projects-pane.md)'s bottom panel together with the account name and a per-window usage bar (5h, 7d); cwd and branch moved to the right-hand [Inspector pane](./inspector.md)'s `GIT` section (which also surfaces the per-file diff stats). The live todo list **used to sit above the input** as the `todo` panel (closed = 1-line compact, open = up to 5 rows, toggle on <kbd>Ctrl+T</kbd>); that panel is gone too - todos now render in the Inspector pane as its `TASKS` section.

One `forge` process owns a config dir and drives every session inside it - a second `forge` on the same config dir is refused at boot with the holder's PID. The [Projects pane](./projects-pane.md) is the in-process coordinator view: every project, its live workers, and the session currently in the chat frame all belong to that one process.

<div class="term">
  <div class="term-bar"><span class="dot r"></span><span class="dot y"></span><span class="dot g"></span><span class="label">forge · standard window</span></div>

  <pre class="indent">
  <span class="dim bold">User</span>
  <span class="user-band">  Read the rate-limit code and add a softer wording branch.               </span>

  Assistant prose, full markdown rendering. Code blocks, lists, inline emphasis,
  links - all handled via <span class="dim">tui-markdown</span> / <span class="dim">syntect</span>.

  <span class="success">✓</span> <span class="bold">⬚</span> <span class="bold">Read</span> crates/forge-tui/src/app.rs
  <span class="success">✓</span> <span class="bold">▣</span> <span class="bold">Edit</span> crates/forge-tui/src/app.rs (+12, -3)

  <span class="success">✓</span> <span class="bold">▶</span> <span class="bold">Bash</span> cargo nextest run -p forge-tui
  <span class="dim">│  </span>$ cargo nextest run -p forge-tui
  <span class="dim">│  </span>   <span class="dim">Compiling forge-tui v0.14.2</span>
  <span class="dim">└─ </span><span class="success">Summary [4.2s] 1565 tests run: 1565 passed</span>
  <span class="dim">8.7s</span>
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                        <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span>
</pre>

</div>

*Left padding is 2 cols across the chat body. The input is a thick-bordered RUST_ORANGE box; its own top and bottom edges are the dividers - no separator rows. The chat scrollbar (overflow only) is a `▐` thumb in `RUST_ORANGE` with no track, in a 1-col gutter reserved off the body's right edge (`CHAT_SCROLLBAR_WIDTH`). The bottom of the chat frame ends at the box's lower edge; the status pieces that used to sit below the input now live in the [Projects pane](./projects-pane.md)'s bottom panel, and the todo list that used to sit above the input is now in the [Inspector pane](./inspector.md)'s `TASKS` section.*
