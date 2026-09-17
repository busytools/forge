//! Identifiers + the `SessionTarget` enum used to address sessions.

// `SessionSlot` lives in forge-primitives so the same routing key
// flows through the TUI → workspace → agent layers without each crate
// growing its own near-identical newtype. Re-exported here so call
// sites continue to import via `forge_workspace::SessionSlot`.
pub use forge_primitives::SessionSlot;

// `ProjectKey` lives in forge-primitives for the same reason
// `SessionSlot` does: the gateway's account selection keys on it and the
// workspace keys its own maps on it. Re-exported here so call sites
// continue to import via `forge_workspace::ProjectKey`.
pub use forge_primitives::ProjectKey;

/// What [`crate::Workspace::get_agent_handle`] should hand back.
#[derive(Clone, Debug)]
pub enum SessionTarget {
    /// The lead of the project marked `default = true` in
    /// `forge.toml`. Errors if no default is configured.
    Default,
    /// Open the lead of the project whose `name` matches the given
    /// string in `forge.toml`. Errors with `ProjectNotFound` if no
    /// such name exists.
    Named(String),
    /// A specific session slot, resumed under the id the store holds
    /// for it. Used by the click-to-resume flow in the Projects pane
    /// and by `Workspace::spawn_session`.
    Session(SessionSlot),
    /// Spawn a FRESH session in `slot`'s project, bypassing the
    /// lead-resume path. Used by the workers MCP so a worker is always
    /// a brand-new session, not a resume of the project's existing
    /// lead. The caller has already recorded the id it minted for the
    /// slot, so the child adopts exactly that one.
    FreshInProject { slot: SessionSlot },
}
