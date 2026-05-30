//! In-process MCP server forge exposes to every spawned `claude`
//! subprocess.
//!
//! The single MCP server is named `forge` and grouped by submodule:
//!
//! - `peers` (#114 v1)  -  cross-agent ask / tell / list / whoami.
//!   Tools are named `peers__ask_agent`, `peers__tell_agent`,
//!   `peers__list_agents`, `peers__whoami`. From the LLM's view they
//!   render as `mcp__forge__peers__ask_agent` and similar.
//! - `workers` - project-internal child-agent coordination (spawn /
//!   list / tell / ask). Tools render as `mcp__forge__workers__<name>`.
//!
//! Tool surface depends on the calling session's kind:
//!
//! - **Lead** sessions (project leads, including peer-spawned project
//!   sessions) see BOTH `peers__*` and `workers__*`. The lead is the
//!   project's representative in cross-project coordination and the
//!   only role that can spawn workers.
//! - **Worker** sessions see ONLY `workers__*`. Cross-project chatter
//!   is the lead's role; a worker that needs cross-project info
//!   surfaces the need to the lead and lets the lead drive the peer
//!   round-trip. Workers retain `workers__*` so they can talk to
//!   their peer workers within the same project (a worker can ask
//!   sibling workers, the lead can spawn / close them).
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

/// Identifies which kind of session the MCP server is being built
/// for. Drives the tool-surface filter in [`build_forge_server`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Project lead  -  the session representing a project in cross-
    /// project coordination. Sees both `peers__*` and `workers__*`.
    Lead,
    /// Worker  -  a project-internal child agent spawned by the lead.
    /// Sees only `workers__*` (no cross-project peer tools).
    Worker,
}

/// Build the per-session `forge` MCP server. ONE McpServer named
/// `forge` carrying the coordination tool groups appropriate for the
/// calling session's [`SessionKind`]:
///
/// - [`SessionKind::Lead`] → peers + workers (eight tools).
/// - [`SessionKind::Worker`] → workers only (four tools).
///
/// Both submodules share the server name when present so the LLM
/// sees a single namespace (`mcp__forge__<group>__*`) and the
/// auto-approve fast-path in `forge-sdk::control_dispatch` matches
/// every tool group with one `mcp__forge__` prefix check.
///
/// Building two separate `McpServer::builder("forge")` instances and
/// pushing both into `OptionsBuilder::mcp_server` would collide on
/// the duplicate name and the CLI would reject one - so the right
/// shape is to combine the (selected) tool sets into a single
/// builder here.
pub fn build_forge_server(
    workspace_facade: Arc<dyn WorkspaceFacade>,
    worker_facade: Arc<dyn WorkerFacade>,
    caller_key: CallerKeyResolver,
    kind: SessionKind,
) -> McpServer {
    let mut builder = McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION"));
    if matches!(kind, SessionKind::Lead) {
        builder = peers::add_tools(builder, workspace_facade, caller_key.clone());
    }
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
    fn build_forge_server_lead_registers_all_eight_tools() {
        let workspace_facade = MockWorkspaceFacade::new().into_arc();
        let worker_facade = MockWorkerFacade::new().into_arc();
        let resolver = CallerKeyResolver::from_fixed(fake_key("test"));
        let server =
            build_forge_server(workspace_facade, worker_facade, resolver, SessionKind::Lead);
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
                "lead build_forge_server must include {expected}; debug: {debug}",
            );
        }
        // Server name is `forge` so tools render as `mcp__forge__<name>`
        // on the LLM side and the SDK auto-approve fast-path covers both
        // groups with one `mcp__forge__` prefix check.
        assert!(debug.contains("name: \"forge\""), "server name must be 'forge'; debug: {debug}");
    }

    #[test]
    fn build_forge_server_worker_registers_only_workers_tools() {
        let workspace_facade = MockWorkspaceFacade::new().into_arc();
        let worker_facade = MockWorkerFacade::new().into_arc();
        let resolver = CallerKeyResolver::from_fixed(fake_key("test"));
        let server =
            build_forge_server(workspace_facade, worker_facade, resolver, SessionKind::Worker);
        let debug = format!("{server:?}");
        // Workers MUST see the four workers__* tools so they can talk
        // to their sibling workers within the same project.
        for expected in ["workers__spawn", "workers__list", "workers__tell", "workers__ask"] {
            assert!(
                debug.contains(expected),
                "worker build_forge_server must include {expected}; debug: {debug}",
            );
        }
        // Workers MUST NOT see peers__*  -  cross-project coordination
        // is a lead-only role; advertising those tools to a worker
        // dumps a non-functional surface on the worker LLM that errors
        // out at call time (the CallerKeyResolver can't map a worker
        // session to a peer identity).
        for forbidden in
            ["peers__whoami", "peers__list_agents", "peers__tell_agent", "peers__ask_agent"]
        {
            assert!(
                !debug.contains(forbidden),
                "worker build_forge_server must NOT include {forbidden}; debug: {debug}",
            );
        }
    }
}
