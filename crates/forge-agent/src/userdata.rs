//! User-data state on disk. Settings, trust, sessions catalog,
//! memory, plugins, slash commands, MCP config — anything the agent
//! reads or writes outside live SDK sessions.

pub mod plugins;
pub mod trust;
