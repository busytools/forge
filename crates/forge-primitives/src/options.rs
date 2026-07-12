//! Pure-data option enums shared across forge-* crates.
//!
//! `Options` itself stays in forge-sdk because it carries callback
//! `Arc<dyn …>` fields - but every wire-shape enum it embeds is data
//! that the agent + UI also need to reason about.

use serde::{Deserialize, Serialize};

pub use crate::permission::PermissionMode;

/// System-prompt configuration. Wraps the CLI's discriminated union of
/// `str | SystemPromptPreset | SystemPromptFile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptKind {
    /// Plain string override - `--system-prompt <text>`.
    Inline(String),
    /// Preset (currently only `claude_code`) with optional append + the
    /// `exclude_dynamic_sections` signal that rides along inside the
    /// `initialize` `control_request` instead of argv.
    Preset {
        /// Optional append text that lands on argv as
        /// `--append-system-prompt <text>`.
        append: Option<String>,
        /// When `Some`, sent in the `initialize` body as
        /// `excludeDynamicSections`. `None` omits the field, matching
        /// the CLI's conditional.
        exclude_dynamic_sections: Option<bool>,
    },
    /// File-backed prompt - `--system-prompt-file <path>`.
    File(std::path::PathBuf),
}

impl SystemPromptKind {
    /// Convenience constructor for the `claude_code` preset with an
    /// append string. Wire shape:
    /// `{"type": "preset", "preset": "claude_code", "append": ...}`.
    pub fn preset_append(append: impl Into<String>) -> Self {
        Self::Preset { append: Some(append.into()), exclude_dynamic_sections: None }
    }
}

/// Plugin config. Wraps the CLI's `SdkPluginConfig`
/// (`{"type": "local", "path": str}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SdkPluginConfig {
    /// Local filesystem plugin - emits `--plugin-dir <path>`.
    Local {
        /// Plugin directory path.
        path: std::path::PathBuf,
    },
}
