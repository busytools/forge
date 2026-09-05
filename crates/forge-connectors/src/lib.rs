//! `forge-connectors` - one module per inbound connector.
//!
//! Each connector module owns the network client, payload mapping and
//! matching rules for one external integration. The [`GotifyHost`]
//! port, implemented by forge-workspace, is the only workspace state,
//! delivery or TLS-trust plumbing a connector may reach, so a
//! connector stays stream + mapping and is testable offline. One
//! connector exists today, so there is deliberately no generic
//! connector trait yet.

pub mod gotify;
