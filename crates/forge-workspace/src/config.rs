//! `forge.toml` schema + loader. Implementation lands in Task 3.

use std::path::PathBuf;

use serde::Deserialize;

/// On-disk schema for `<config_dir>/forge.toml` in 1a. `[[accounts]]`
/// and `[selection]` arrive in 1b.
#[derive(Debug, Deserialize)]
pub(crate) struct ForgeToml {
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProjectEntry {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub default: bool,
}

/// In-memory representation after path expansion + default-project
/// selection.
#[derive(Debug, Clone)]
pub(crate) struct LoadedConfig {
    pub projects: Vec<LoadedProject>,
    pub default_index: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedProject {
    pub name: String,
    pub path: PathBuf,
}

impl LoadedConfig {
    pub(crate) fn default_project(&self) -> &LoadedProject {
        &self.projects[self.default_index]
    }
}
