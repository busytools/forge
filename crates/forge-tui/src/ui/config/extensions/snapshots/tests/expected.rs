//! The pinned Extensions-page render snapshots, one per tab, captured
//! at 160x40. Each is the exact frame text with the scaffold's right
//! box edge and trailing whitespace dropped per row. These are the
//! visual contract the snapshot tests enforce.

pub(crate) const INSTALLED: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)   Filter by name, plugin or marketplace
│  >✓ superpowers   superpowers-market   installed 6.3.0                                    ~450 tok always-on
│   ⚠ rust-review   code-review-market   1.1.0 -> 1.2.0 available                           ~120 tok always-on                                         Update
│   ✓ leyline       claude-night-market  installed 0.1.0                                    [auto-installed]
│   ✗ off-plugin    probe-market         disabled
│   ✗ broken-ghost  ghost-market         failed: registered install dir is missing on disk
│   ✓ lsp-support   probe-market         installed 1.0.0
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
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | u update all | c check updates | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const SKILLS: &str = r"
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
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const SKILLS_AVAILABLE: &str = r"
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
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | Esc close
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
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | Esc close
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
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | Esc close
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
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | Esc close
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
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | a available | u update all | c check updates | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const MCPS: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│
│
│     total 1   connected 1   needs auth 0   pending 0   disabled 0   failed 0
│
│    > plugin:context7:context7   connected   user   stdio
│      Context7 1.0.0  |  1 tool  |  cmd npx
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
│Left/Right switch tab | Up/Down move | Enter actions | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const MARKETPLACES: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│   superpowers-market  healthy · 12 plugins  github
│   claude-night-market  registry drift - installLocation outside the config dir  Repair  github
│   ghost-market  load failed: no marketplace clone on disk  Repair  github
│  Add marketplace
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
│Left/Right switch tab | Up/Down move | Enter actions | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";

pub(crate) const UPDATES: &str = r"
┌Extensions────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│
│  Installed 6   Skills 10   Agents 2   Commands 3   Hooks 1   LSP 1   MCPs 1   Marketplaces 3
│  Update all (u) (1)   Filter by name, plugin or marketplace
│  >✓ superpowers   superpowers-market   installed 6.3.0                                    ~450 tok always-on
│   ⚠ rust-review   code-review-market   1.1.0 -> 1.2.0 available                           ~120 tok always-on                                         Update
│   ✓ leyline       claude-night-market  installed 0.1.0                                    [auto-installed]
│   ✗ off-plugin    probe-market         disabled
│   ✗ broken-ghost  ghost-market         failed: registered install dir is missing on disk
│   ✓ lsp-support   probe-market         installed 1.0.0
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
│  Updates - 1 of 1 done · restart required to apply
│    rust-review@code-review-market  done
│
│
│
│Left/Right switch tab | Up filter | Up/Down move | Enter actions | u update all | c check updates | Esc close
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘";
