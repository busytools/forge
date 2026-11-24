//! The `[server]` section of `forge.toml`.

use std::net::{IpAddr, Ipv4Addr};

use serde::Deserialize;

/// The `[server]` block of `forge.toml`: whether the socket's listener
/// starts with forge, where it binds, how large a served page's structured
/// tool record may be, and the startup gate the boot runs first.
///
/// On unless it is turned off, so a restart leaves the socket serving
/// without a key being added first.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub enabled: bool,
    pub port: u16,
    /// Loopback by default: an address on a network interface is a
    /// deliberate line in `forge.toml`, never something on-by-default
    /// reaches.
    pub bind: IpAddr,
    /// How large a frame's `tool_use_result` may be in anything a client is
    /// served, in KB (#2374).
    ///
    /// **A record over it is reduced first**, keeping only the keys a client
    /// draws (a mutation's patch and outside-edit mark, a question's answers
    /// and notes); it becomes an `omitted` marker only when that reduction is
    /// still over the cap. Every path to a client is bounded - a page, a
    /// `more`, the live stream and both repairs - while the transcript keeps
    /// the record whole and the visible tool-result block draws as always.
    /// `0` serves the records whole, for a reader whose changes carry patches
    /// beyond the default.
    pub tool_record_cap_kb: u64,
    /// How many bytes of conversation a session's first snapshot carries, in
    /// KB.
    ///
    /// **The first snapshot is what a seat switch waits on**, and twenty
    /// turns of a large conversation are megabytes: seconds over a network,
    /// from a frame a client spends milliseconds on locally. The newest
    /// turns are kept while they fit and the rest is paged through `more` as
    /// it always was, so no turn is dropped and the order does not move. A
    /// turn that alone is over the budget is served in pages of its own
    /// rather than whole, so the reader's fold is bounded and the history
    /// above stays reachable through `more`. The bytes counted are the bytes
    /// SERVED: a turn's repeated progress frames are pruned to one copy each
    /// before the cut, while every lifecycle frame and a workflow's
    /// `task_progress` tick are served every time because the live path draws
    /// a row for each.
    /// `0` turns off the byte cut (no turn is split), not the pruning, and
    /// serves the page the turn count asks for, for a reader who would
    /// rather have the whole window in one frame.
    pub first_frame_cap_kb: u64,
    /// How many bytes of conversation a `more` page carries, in KB.
    ///
    /// **A page past the first frame is unbounded by its turn count alone**:
    /// one large turn could cross a `more` answer as tens of megabytes. The
    /// newest turns are kept while they fit; a turn over the budget is served
    /// in pages of its own, its newest frames first, and the cursor names the
    /// row above the page - no turn is dropped, and the walk reaches every
    /// turn and every row the pages serve. The bytes counted are the bytes
    /// served, after the repeated progress frames are pruned.
    /// `0` serves the page the turn count asks for, whole and unsplit, with
    /// the pruning still applied.
    pub more_page_cap_kb: u64,
    /// Extra browser origins allowed to open the socket, beside the shell's
    /// own, the server's own, and any page served on the user's own machine -
    /// the `localhost` name or any loopback IP address, any port, http or
    /// https, in every build. A web client served from another address is one
    /// only the operator knows; an empty list allows none.
    pub allowed_origins: Vec<String>,
    /// An executable forge runs at startup, before any listener binds, and
    /// waits on with no timeout; a non-zero exit fails the boot and forge
    /// exits non-zero.
    ///
    /// **The one reason this key exists is macOS Transparency, Consent, and
    /// Control (TCC).** TCC attributes a file access to the process launchd
    /// spawned, so a daemon launched through `bash` makes every worker's
    /// Bash tool attribute to bash and a Full Disk Access grant on the forge
    /// binary never applies. Running forge directly as the daemon's program
    /// fixes that, and this key is what lets the machine's own infra gate
    /// still run first. The value is a path to an executable script the
    /// operator supplies; forge only runs it, and the child inherits forge's
    /// stdout and stderr. It runs once, on the boot path, and never again
    /// once serving has started.
    pub startup_gate: Option<String>,
}

/// The port the server binds when `[server] port` is absent. Distinct
/// from the gateway's 8787, the other listener a boot brings up.
pub const DEFAULT_SERVER_PORT: u16 = 8790;

/// The served `tool_use_result` cap when `[server] tool_record_cap_kb` is
/// absent. See `ServerConfig::tool_record_cap_kb`.
pub const DEFAULT_TOOL_RECORD_CAP_KB: u64 = 64;

/// The first snapshot's conversation budget when `[server]
/// first_frame_cap_kb` is absent. See `ServerConfig::first_frame_cap_kb`.
pub const DEFAULT_FIRST_FRAME_CAP_KB: u64 = 512;

/// A `more` page's conversation budget when `[server] more_page_cap_kb` is
/// absent. See `ServerConfig::more_page_cap_kb`.
pub const DEFAULT_MORE_PAGE_CAP_KB: u64 = 512;

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_SERVER_PORT,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            tool_record_cap_kb: DEFAULT_TOOL_RECORD_CAP_KB,
            first_frame_cap_kb: DEFAULT_FIRST_FRAME_CAP_KB,
            more_page_cap_kb: DEFAULT_MORE_PAGE_CAP_KB,
            allowed_origins: Vec::new(),
            startup_gate: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped default, and the opt-out: absent means 64 KB, and `0`
    /// means the records cross whole. **The default is pinned as a literal**,
    /// because the book documents 64 and comparing it to the constant would
    /// move with it.
    #[test]
    fn the_served_record_cap_defaults_to_64_kb_and_zero_means_whole() {
        assert_eq!(ServerConfig::default().tool_record_cap_kb, 64, "the documented default");
        assert_eq!(DEFAULT_TOOL_RECORD_CAP_KB, 64, "and the constant the default is built from");
        let parsed: ServerConfig =
            serde_json::from_str(r#"{"tool_record_cap_kb": 0}"#).expect("the key parses");
        assert_eq!(parsed.tool_record_cap_kb, 0);
    }

    /// The shipped default, and the opt-out: absent means a 512 KB `more`
    /// page, and `0` means the turn count alone decides it. Pinned as a
    /// literal for the same reason as the other caps'.
    #[test]
    fn the_more_page_cap_defaults_to_512_kb_and_zero_means_the_count() {
        assert_eq!(ServerConfig::default().more_page_cap_kb, 512, "the documented default");
        assert_eq!(DEFAULT_MORE_PAGE_CAP_KB, 512, "and the constant the default is built from");
        let parsed: ServerConfig =
            serde_json::from_str(r#"{"more_page_cap_kb": 0}"#).expect("the key parses");
        assert_eq!(parsed.more_page_cap_kb, 0);
    }

    /// The shipped default, and the opt-out: absent means a 512 KB first
    /// page, and `0` means the turn count alone decides it. Pinned as a
    /// literal for the same reason as the record cap's.
    #[test]
    fn the_first_frame_cap_defaults_to_512_kb_and_zero_means_the_count() {
        assert_eq!(ServerConfig::default().first_frame_cap_kb, 512, "the documented default");
        assert_eq!(DEFAULT_FIRST_FRAME_CAP_KB, 512, "and the constant the default is built from");
        let parsed: ServerConfig =
            serde_json::from_str(r#"{"first_frame_cap_kb": 0}"#).expect("the key parses");
        assert_eq!(parsed.first_frame_cap_kb, 0);
    }

    /// The gate is opt-in: an absent key runs nothing, and a set one carries
    /// the path the boot executes.
    #[test]
    fn the_startup_gate_is_unset_until_the_file_sets_one() {
        assert_eq!(ServerConfig::default().startup_gate, None, "absent means no gate");
        let parsed: ServerConfig =
            serde_json::from_str(r#"{"startup_gate": "/opt/gate.sh"}"#).expect("the key parses");
        assert_eq!(parsed.startup_gate.as_deref(), Some("/opt/gate.sh"));
    }
}
