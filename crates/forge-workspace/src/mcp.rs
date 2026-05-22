//! In-process MCP server forge exposes to every spawned `claude`
//! subprocess.
//!
//! The single MCP server is named `forge` and grouped by submodule:
//!
//! - `peers` (#114 v1) — cross-agent ask / tell / list / whoami.
//!   Tools are named `peers__ask_agent`, `peers__tell_agent`,
//!   `peers__list_agents`, `peers__whoami`. From the LLM's view they
//!   render as `mcp__forge__peers__ask_agent` and similar.
//! - `workers` - project-internal child-agent coordination (spawn /
//!   list / tell / ask). Tools render as `mcp__forge__workers__<name>`.
//!
//! Future submodules slot in alongside these (e.g. `worktree`,
//! `memory`) without changing the server name or the auto-approve
//! fast-path in `forge-sdk::control_dispatch` (which matches the
//! `mcp__forge__` prefix at the tool-name level).

use std::sync::Arc;

use forge_sdk::mcp::server::{McpServer, McpServerBuilder};

use crate::mcp::peers::facade::{CallerKeyResolver, WorkspaceFacade};
use crate::mcp::workers::facade::WorkerFacade;

pub mod peers;
pub mod workers;

/// Build the per-session `forge` MCP server. ONE McpServer named
/// `forge` carrying all peer-coordination + workers-coordination
/// tools. Both submodules share the server name so the LLM sees a
/// single namespace (`mcp__forge__peers__*` and
/// `mcp__forge__workers__*`) and the auto-approve fast-path in
/// `forge-sdk::control_dispatch` matches both with one prefix check.
///
/// Building two separate `McpServer::builder("forge")` instances and
/// pushing both into `OptionsBuilder::mcp_server` would collide on
/// the duplicate name and the CLI would reject one - so the right
/// shape is to combine the tool sets into a single builder here.
pub fn build_forge_server(
    workspace_facade: Arc<dyn WorkspaceFacade>,
    worker_facade: Arc<dyn WorkerFacade>,
    caller_key: CallerKeyResolver,
) -> McpServer {
    let mut builder = McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION"));
    builder = peers::add_tools(builder, workspace_facade, caller_key.clone());
    builder = workers::add_tools(builder, worker_facade, caller_key);
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionKey;
    use crate::mcp::peers::facade::MockWorkspaceFacade;
    use crate::mcp::workers::facade::MockWorkerFacade;

    fn fake_key(s: &str) -> SessionKey {
        SessionKey::from_session_id(s)
    }

    #[test]
    fn build_forge_server_registers_all_eight_tools() {
        let workspace_facade = MockWorkspaceFacade::new().into_arc();
        let worker_facade = MockWorkerFacade::new().into_arc();
        let resolver = CallerKeyResolver::from_fixed(fake_key("test"));
        let server = build_forge_server(workspace_facade, worker_facade, resolver);
        let debug = format!("{server:?}");
        for expected in [
            "peers__whoami",
            "peers__list_agents",
            "peers__tell_agent",
            "peers__ask_agent",
            "workers__spawn",
            "workers__list",
            "workers__tell",
            "workers__ask",
        ] {
            assert!(
                debug.contains(expected),
                "build_forge_server must include {expected}; debug: {debug}",
            );
        }
        // Server name is `forge` so tools render as `mcp__forge__<name>`
        // on the LLM side and the SDK auto-approve fast-path covers both
        // groups with one `mcp__forge__` prefix check.
        assert!(debug.contains("name: \"forge\""), "server name must be 'forge'; debug: {debug}");
    }
}
