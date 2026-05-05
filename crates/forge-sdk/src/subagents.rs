//! Subagent declarations — re-export shim.
//!
//! Wire-shape data (`SubagentDefinition`, `SubagentMemory`,
//! `SubagentMcpServerRef`, `EffortLevel`, `EffortPreset`, `SubagentMap`)
//! lifted to forge-primitives in 2026-05-05.

pub use forge_primitives::subagents::{
    EffortLevel, EffortPreset, SubagentDefinition, SubagentMap, SubagentMcpServerRef,
    SubagentMemory,
};
