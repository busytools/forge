//! The pinned Extensions-page render snapshots, one per tab, captured
//! at 160x40. Each is the exact frame text with the scaffold's right
//! box edge and trailing whitespace dropped per row. These are the
//! visual contract the snapshot tests enforce.

// Justification: the Installed tab now carries the available catalog
// after the installed rows (decision 3), so its pin gains the
// fixture's two available plugin rows with Install actions, the +2
// beside the Available chip, and the `a available` help segment.
pub(crate) const INSTALLED: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +2   Filter by name, plugin or marketplace
│  >✓ superpowers   superpowers-market   installed 6.3.0                                    ~450 tok always-on
│   ⚠ rust-review   code-review-market   1.1.0 -> 1.2.0 available                           ~120 tok always-on                                         Update
│   ✓ leyline       claude-night-market  installed 0.1.0                                    [auto-installed]
│   ✗ off-plugin    probe-market         disabled
│   ✗ broken-ghost  ghost-market         failed: registered install dir is missing on disk
│   ✓ lsp-support   probe-market         installed 1.0.0
│   - blabbermouth  claude-night-market  available 1.9.19 - not installed                                                                             Install
│   - sec-audit     trailofbits          available 1.2.0 - not installed                                                                              Install
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

// Justification: the Available toggle is now a hide (the available
// stream renders by default after the installed rows), so the default
// Skills pin is the former toggle-on render, and the toggle-on pin is
// replaced by this toggle-off pin holding the former default render.
pub(crate) const SKILLS_HIDDEN: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +3   Filter by name, plugin or marketplace
│  >✓ brainstorming                 superpowers  installed 6.3.0
│   ✓ executing-plans               superpowers  installed 6.3.0
│   ✓ systematic-debugging          superpowers  installed 6.3.0
│   ✓ test-driven-development       superpowers  installed 6.3.0
│   ✓ using-superpowers             superpowers  installed 6.3.0
│   ✓ verification-before-complet…  superpowers  installed 6.3.0
│   ⚠ architecture-review           rust-review  1.1.0 -> 1.2.0 available                                                                              Update
│   ⚠ rust-review                   rust-review  1.1.0 -> 1.2.0 available                                                                              Update
│   ✓ stewardship                   leyline      installed 0.1.0
│   ✗ old-thing                     off-plugin   disabled
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const SKILLS: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +3   Filter by name, plugin or marketplace
│  >✓ brainstorming                 superpowers   installed 6.3.0
│   ✓ executing-plans               superpowers   installed 6.3.0
│   ✓ systematic-debugging          superpowers   installed 6.3.0
│   ✓ test-driven-development       superpowers   installed 6.3.0
│   ✓ using-superpowers             superpowers   installed 6.3.0
│   ✓ verification-before-complet…  superpowers   installed 6.3.0
│   ⚠ architecture-review           rust-review   1.1.0 -> 1.2.0 available                                                                             Update
│   ⚠ rust-review                   rust-review   1.1.0 -> 1.2.0 available                                                                             Update
│   ✓ stewardship                   leyline       installed 0.1.0
│   ✗ old-thing                     off-plugin    disabled
│   - announce                      blabbermouth  available 1.9.19 - not installed                                                                    Install
│   - deduplicate                   blabbermouth  available 1.9.19 - not installed                                                                    Install
│   - sec-audit                     sec-audit     available 1.2.0 - not installed                                                                     Install
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const AGENTS: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +0   Filter by name, plugin or marketplace
│  >✓ brainstormer  superpowers  installed 6.3.0
│   ✓ plan-writer   superpowers  installed 6.3.0
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const COMMANDS: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +0   Filter by name, plugin or marketplace
│  >✓ brainstorm    superpowers  installed 6.3.0
│   ✓ write-plan    superpowers  installed 6.3.0
│   ⚠ review        rust-review  1.1.0 -> 1.2.0 available                                                                                              Update
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const HOOKS: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +0   Filter by name, plugin or marketplace
│  >✓ superpowers   superpowers  installed 6.3.0           SessionStart, PreToolUse
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const LSP: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +0   Filter by name, plugin or marketplace
│  >✓ rust-analyzer  lsp-support  installed 1.0.0           rust-analyzer: on PATH
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

// Justification: the Mcps tab joins the shared row grammar - one row
// per server (state glyph from the connection status, name, scope as
// source, status column, transport badge, summary as detail) with the
// selection marker on the row's leading gutter, replacing the old
// renderer's two-line rows and summary badge line.
pub(crate) const MCPS: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  >✓ plugin:context7:context7  user        connected                 [stdio]  Context7 1.0.0  |  1 tool  |  cmd npx
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up/Down move | Enter actions | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

// Justification: the Marketplaces tab joins the shared row grammar -
// health as the glyph, source kind as the source column,
// `healthy - N plugins` (or the truncated drift notice / failure
// reason) in the status column, Repair right-aligned on the rows that
// qualify, the repo as dim detail, and the add row carrying a leading
// gutter so the marker can take it in place.
pub(crate) const MARKETPLACES: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  >✓ superpowers-market   github      healthy - 12 plugins
│   ⚠ claude-night-market  github      registry drift - installLocation outside the config…                                                            Repair
│   ✗ ghost-market         github      failed: no marketplace clone on disk                                                                            Repair
│   Add marketplace
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│Left/Right switch tab | Up/Down move | Enter actions | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

// Justification: this pin renders on the Installed tab, so it picks
// up the same change as INSTALLED - the fixture's two available
// plugin rows with Install actions, the +2 chip, and the `a available`
// help segment - under the docked update panel.
pub(crate) const UPDATES: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)  Available (a) +2   Filter by name, plugin or marketplace
│  >✓ superpowers   superpowers-market   installed 6.3.0                                    ~450 tok always-on
│   ⚠ rust-review   code-review-market   1.1.0 -> 1.2.0 available                           ~120 tok always-on                                         Update
│   ✓ leyline       claude-night-market  installed 0.1.0                                    [auto-installed]
│   ✗ off-plugin    probe-market         disabled
│   ✗ broken-ghost  ghost-market         failed: registered install dir is missing on disk
│   ✓ lsp-support   probe-market         installed 1.0.0
│   - blabbermouth  claude-night-market  available 1.9.19 - not installed                                                                             Install
│   - sec-audit     trailofbits          available 1.2.0 - not installed                                                                              Install
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│
│  Updates - 1 of 1 done · restart required to apply
│    rust-review@code-review-market  done
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | r refresh | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";
