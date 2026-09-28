//! Model state shared by more than one of the model's own modules.

// A monitor's status crosses the agent/workspace/sessions boundary, so
// it lives in primitives and is named here for the modules that had it.
pub use forge_primitives::MonitorStatus;
