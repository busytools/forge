//! Cloud-side wire shape types.
//!
//! Type-only  -  the actual HTTP / keychain / filesystem fetchers live
//! in `forge_agent::cloud::*`. These are the types that cross crate
//! boundaries.

pub mod oauth_credentials;
pub mod service_status;
