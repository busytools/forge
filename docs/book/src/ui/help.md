# Help and welcome

## Help overlay (`?`)

Toggled with <kbd>?</kbd>: a rounded panel above the input area, fixed 14 rows with up to 10 content rows. The title is a rust-orange bold `Help` followed by bracketed tabs `[Keys | Slash | Subagents]` - the active tab rust orange bold, the rest dim - plus a dim hint suffix (`(< > switch tabs)` on Keys; `(< > tabs  ▲▼ scroll)` on Slash and Subagents). Three views:

- **Keys** - items in two half-columns, each cell `label : description` with a bold label and a dim separator.
- **Slash commands** - a two-column list of built-ins (`/config`, `/effort`, `/mcp`, `/plugins`) plus user-installed commands.
- **Subagents** - the same two-column shape, listing user-defined subagents.

<div class="term">

  <pre class="indent">
  <span class="dim">╭</span> <span class="accent bold">Help</span> <span class="dim">[</span><span class="accent bold">Keys</span><span class="dim"> | </span><span class="dim">Slash</span><span class="dim"> | </span><span class="dim">Subagents</span><span class="dim">]</span>  <span class="dim">(&lt; &gt; switch tabs)</span> <span class="dim">─────────────────╮</span>
  <span class="dim">│</span>                                                                     <span class="dim">│</span>
  <span class="dim">│</span>     <span class="bold">↑ / ↓</span><span class="dim"> : </span>Scroll chat       <span class="bold">Esc</span><span class="dim"> : </span>Cancel current action          <span class="dim">│</span>
  <span class="dim">│</span>     <span class="bold">PgUp / PgDn</span><span class="dim"> : </span>Page         <span class="bold">Ctrl+C</span><span class="dim"> : </span>Interrupt response          <span class="dim">│</span>
  <span class="dim">│</span>     <span class="bold">Ctrl+X</span><span class="dim"> : </span>Expand tool      <span class="bold">?</span><span class="dim"> : </span>Toggle help                    <span class="dim">│</span>
  <span class="dim">│</span>                                                                     <span class="dim">│</span>
  <span class="dim">╰─────────────────────────────────────────────────────────────────────╯</span></pre>

</div>

| Key | Action |
|---|---|
| <kbd><</kbd> / <kbd>></kbd> | Switch tabs |
| <kbd>↑</kbd> <kbd>↓</kbd> | Scroll within the Slash / Subagents lists |

<details>
<summary>Platform-aware bindings</summary>

Input-editing shortcuts swap modifiers per OS: macOS uses `Cmd+Z` / `Cmd+Shift+Z` (undo / redo), `Alt+Left/Right` (word nav), `Alt+Backspace/Delete` (word delete), and `Cmd+C` / `Cmd+V` for copy / paste; Linux and Windows use `Ctrl+` for all of these. `Ctrl+C` still works as fallback copy + interrupt-on-empty everywhere. Reaching the app on macOS requires the kitty enhanced-keyboard protocol (Ghostty / kitty / WezTerm); forge negotiates the flags at startup and again on every resize, since a byte-transparent session manager leaves them on the terminal a reattach left behind.

</details>

## Welcome

The first message in a fresh chat - a regular scrollback message, not an overlay: a rust-orange bold "Overview" banner over a Ferris-says ASCII block (rust orange), a metadata block (Version, Account, cwd, Session ID), and one rotating tip. The account line shows `Account: <display name> · <tier>` when workspace routing picked the account from `forge.toml`, falling back to `Subscription: <tier>` otherwise.

<div class="term">

  <pre class="indent">
  <span class="accent-bold">Overview</span>

  <span class="accent">--------------------------------- </span>
  <span class="accent">&lt; Welcome back to Claude, in Rust! &gt;</span>
  <span class="accent">--------------------------------- </span>
  <span class="accent">        \             </span>
  <span class="accent">         \            </span>
  <span class="accent">            _~^~^~_  </span>
  <span class="accent">        \) /  o o  \ (/</span>
  <span class="accent">          '_ - _' </span>
  <span class="accent">          / '-----' \ </span>


  <span class="dim">Version:      </span><span class="dim">1.0.53 · 3cda0dee</span>
  <span class="dim">Account:      </span><span class="accent bold">Stargate · team</span>
  <span class="dim">cwd:          ~/Projects/forge</span>
  <span class="dim">Session ID:   550e8400-e29b-41d4-a716-446655440000</span>

  <span class="dim">Tips: Use /mode plan before larger changes, then switch back to code once the plan is clear</span>
</pre>

</div>

- The ASCII art is rust orange; the field labels, the other field values and the tip are dim; the account value is rust orange bold.
