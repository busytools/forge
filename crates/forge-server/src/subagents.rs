//! The session's sub-agent instances: one card per `Task`/`Agent`
//! dispatch, each with the tool calls that ran under it.
//!
//! **The fold moved to `forge-workspace`** (`subagent_cards`), because the
//! session task is the one that can announce the list moving: it folds a
//! frame at a time in the walk that already raises `DispatchesChanged`, and
//! a page that attaches later reads the same fold driven over the
//! conversation the transport holds. Two folds here would have been the
//! drift this module's own docs warn about one level down, so this file is
//! now the wire's import path and nothing else.

pub use forge_primitives::runtime::SubagentCard;
pub use forge_workspace::subagent_cards::{TAIL_CAP, subagent_cards};
