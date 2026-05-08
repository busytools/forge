// In-process integration-style suites built on `App::test_default()` and direct
// `handle_client_event()` calls. These validate multi-event state sequences, not
// an external bridge or terminal boundary.

// Test-only crate. `expect`/`unwrap`/`panic` are the right error-reporting
// paths for fixture setup; `clippy.toml`'s `allow-*-in-tests` only covers
// `#[test]`-annotated fns, so module-level allow is needed for top-level
// helpers.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod helpers;
mod message_helpers;

mod caching_pipeline;
mod internal_failures;
mod permissions;
mod state_transitions;
mod tool_lifecycle;
