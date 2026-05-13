//! Live environment — git context, cwd insights, OS-side
//! observations the agent needs but the SDK doesn't.
//!
//! Distinct from `userdata` (on-disk Claude state) and `cloud`
//! (network-side talk to api.anthropic.com): `env` is local-machine
//! state about the project the user is working on right now.

pub mod git_diff;
