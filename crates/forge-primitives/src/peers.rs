//! Per-session peer-activity counters.
//!
//! This is the one peer-coordination type that genuinely crosses
//! crate boundaries: the workspace owns the master map
//! (`Workspace.peer_stats`), updates it in
//! `WorkspaceFacade::bump_inflight_stats`, and emits
//! `SessionUpdate::PeerInflightStatsChanged` carrying these counters
//! to forge-tui (where the Projects-pane reducer renders sidebar
//! peer-activity badges).
//!
//! The rest of the peer wire-shape types (CorrelationId, InflightAsk,
//! WrappedPrompt, PeerStatus, PeerLiveness, PeerFailureReason,
//! WrappedKind) live in `forge-workspace::mcp::peers::types`  - 
//! they're workspace-internal and don't need to be in primitives.

use serde::{Deserialize, Serialize};

/// Per-session counters of pending peer activity. Maintained at the
/// workspace level (`peer_stats` map) and surfaced via
/// `SessionUpdate::PeerInflightStatsChanged` to drive sidebar badges
/// in the Projects pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInflightStats {
    /// Asks this session has sent to peers, awaiting reply.
    pub outgoing: usize,
    /// Asks this session has received from peers, awaiting our reply.
    pub incoming: usize,
    /// Asks this session sent that timed out without a reply
    /// (visible briefly in the sidebar badge before fading).
    pub timed_out: usize,
    /// Asks this session sent that failed to deliver
    /// (visible briefly in the sidebar badge before fading).
    pub delivery_failed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_inflight_stats_default_is_zero() {
        let s = PeerInflightStats::default();
        assert_eq!(s.outgoing, 0);
        assert_eq!(s.incoming, 0);
        assert_eq!(s.timed_out, 0);
        assert_eq!(s.delivery_failed, 0);
    }
}
