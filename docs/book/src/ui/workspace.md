# Workspace

One `forge` process owns one config dir and drives every session inside it - a second `forge` on the same config dir is refused at boot with the holder's PID. The frame is a fixed vertical stack flanked by the [Projects pane](./projects-pane.md) and the [Inspector pane](./inspector.md); the crate layering underneath is described in [Architecture](../architecture.md).

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
  <span class="dim">│  </span>   <span class="dim">Compiling forge-tui v1.0.53</span>
  <span class="dim">└─ </span><span class="success">Summary [4.2s] 1565 tests run: 1565 passed</span>
  <span class="dim">8.7s</span>
  <span class="accent">┏</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┓</span>
  <span class="accent">┃</span> <span class="accent">➤</span> <span class="dim italic">Type a message...</span>                                                      <span class="accent">┃</span>
  <span class="accent">┗</span><span class="accent">━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━</span><span class="accent">┛</span>
</pre>

</div>

*Left padding is 2 cols across the chat body. The input is a thick-bordered rust-orange box. The chat scrollbar (overflow only) is a `▐` thumb in rust orange with no track, in a 1-col gutter reserved off the body's right edge. The bottom of the chat frame ends at the box's lower edge.*

## Frame

| Terminal width | Shape |
|---|---|
| 160 cols and up | 32ch [Projects pane](./projects-pane.md) on the left, 40ch [Inspector pane](./inspector.md) on the right |
| 120-159 | The panes shrink to 24ch and 30ch, with truncation |
| Under 120 | A single-row top bar `▤  <active-project>·<active-session>` replaces both panes |

| Key | Action |
|---|---|
| `▤` tap, or <kbd>Cmd+Left</kbd> (<kbd>Ctrl+Left</kbd> off macOS) | Open the Projects overlay |
| `▦` tap, or <kbd>Cmd+Right</kbd> (<kbd>Ctrl+Right</kbd> off macOS) | Open the Inspector overlay |

The chat view stacks, top to bottom:

| Region | Shows |
|---|---|
| Body | The chat scrollback: minimum 3 rows, takes the remaining height |
| Input | The bordered box: 1 interior row, growing up to 50 as the user types; its own thick top and bottom edges are the dividers - there are no separator rows |
| Help | 0 when inactive, else a 14-row overlay |

- `/config` swaps the entire frame to the config view - chat, input and both side panes disappear until it closes.
- Mode / Model / Effort and the 5h + 7d usage bars render in the [Projects pane](./projects-pane.md)'s bottom panel; cwd, branch and per-file diff stats render in the [Inspector pane](./inspector.md)'s `GIT` section; todos render as the Inspector's `TASKS`.
- The [Projects pane](./projects-pane.md) is the in-process coordinator view: every project, its live workers, and the session currently in the chat frame all belong to that one process.
