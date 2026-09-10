//! `forge-connectors` - one module per inbound connector.
//!
//! Each connector module owns the network client, payload mapping and
//! matching rules for one external integration. The
//! [`GotifyHost`](gotify::GotifyHost) port, implemented by
//! forge-workspace, is the only workspace state, delivery or TLS-trust
//! plumbing a connector may reach, so a connector stays stream +
//! mapping and is testable offline. Two connectors exist today - Gotify
//! pushes through a host port, Slack is called by the workspace
//! directly - so there is still deliberately no generic connector trait.

pub mod gotify;
pub mod slack;
