//! `forge-tools` - one module per external integration, inbound or outbound.
//!
//! Each module owns the network client, payload mapping and matching rules
//! for one external integration, plus the tool family and held asks that
//! integration brings. What a module needs from its host arrives through
//! that integration's own port, implemented by forge-workspace - Gotify's
//! [`GotifyHost`](gotify::GotifyHost), the Slack family's
//! [`slack::SlackStore`], [`slack::SlackEmit`] and
//! [`slack::facade::SlackFacadeHost`], the vault family's
//! [`vault::VaultHost`] - so each module stays transport + mapping and is
//! testable offline. Three integrations exist today, and there is still
//! deliberately no generic connector trait: the ports differ because what
//! each integration needs differs.

pub mod gotify;
pub mod slack;
pub mod vault;
