//! `forge-tools` - one module per external integration, inbound or outbound.
//!
//! Each module owns the network client, payload mapping and matching rules
//! for one external integration, plus the tool family and held asks that
//! integration brings. The [`GotifyHost`](gotify::GotifyHost) port,
//! implemented by forge-workspace, is the only workspace state, delivery or
//! TLS-trust plumbing a connector may reach, so a connector stays stream +
//! mapping and is testable offline. Three integrations exist today - Gotify
//! pushes through a host port, Slack owns its subscription state, durable
//! records, pumps and held drafts behind [`slack::SlackStore`] and
//! [`slack::SlackEmit`], and the vault family owns its client, its session,
//! its tools and its parked calls behind [`vault::VaultHost`] - so there is
//! still deliberately no generic connector trait.

pub mod gotify;
pub mod slack;
pub mod vault;
