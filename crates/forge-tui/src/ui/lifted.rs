//! Upstream `claude-code-rust` UI files lifted into forge-tui in
//! parallel with the legacy hand-rolled `ui` modules. The legacy path
//! drives the running TUI today; once the lifted set is complete and
//! the upstream-shape `state::app::App` carries enough state, the
//! renderer cuts over and the legacy modules drop.

pub mod autocomplete;
pub mod footer;
pub mod help;
pub mod input;
pub mod session_picker;
pub mod todo;
