//! User-data state on disk. Settings, sessions catalog, memory,
//! plugins, slash commands, MCP config — anything the agent reads
//! or writes outside live SDK sessions.

pub mod catalog;
pub mod memory;
pub mod plugins;
pub mod settings;
