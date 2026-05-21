//! In-process MCP server forge exposes to every spawned `claude`
//! subprocess.
//!
//! The single MCP server is named `forge` and grouped by submodule:
//!
//! - `peers` (#114 v1) — cross-agent ask / tell / list / whoami.
//!   Tools are named `peers__ask_agent`, `peers__tell_agent`,
//!   `peers__list_agents`, `peers__whoami`. From the LLM's view they
//!   render as `mcp__forge__peers__ask_agent` and similar.
//!
//! Future submodules slot in alongside `peers` (e.g. `worktree`,
//! `memory`) without changing the server name or the auto-approve
//! fast-path in `forge-sdk::control_dispatch` (which matches the
//! `mcp__forge__` prefix at the tool-name level).

pub mod peers;
pub mod workers;
