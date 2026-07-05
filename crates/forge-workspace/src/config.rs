//! `forge.toml` schema + loader.
//!
//! **Org model.** Projects are grouped under `[[orgs]]` (Subspace,
//! Granite, Personal, etc.). Each org carries the `accounts = [...]`
//! pin shared by all its projects, replacing the per-project pin
//! that lived here before. Projects within an org keep a flat list
//! via `[[orgs.projects]]`. Multiple projects can carry
//! `auto_start = true`; all auto-start projects spawn at launch and
//! the first one (alphabetical) becomes the focused tab.
//!
//! **Selection policy.** Exactly one: every account spawn picks the
//! account in the org's `accounts` subset with the most remaining
//! usage budget. Cold cache → first-in-subset by definition order.
//! No LRU, no round-robin, no fallback to accounts outside the org's
//! subset.

use std::fs;
use std::path::{Path, PathBuf};

use forge_primitives::GotifyConfig;
use serde::Deserialize;

use crate::error::WorkspaceError;
use crate::ui::UiSettings;

#[derive(Debug, Deserialize)]
struct ForgeToml {
    #[serde(default)]
    orgs: Vec<OrgEntry>,
    #[serde(default)]
    accounts: Vec<AccountEntry>,
    /// Optional `[ui]` section - visual knobs that don't fit on
    /// `[[orgs]]` / `[[accounts]]`. Currently carries the launchpad
    /// spinner style; will grow as the launchpad UI lands. Absent
    /// section → all defaults.
    #[serde(default)]
    ui: UiSettings,
    /// Optional `[gotify]` section - the inbound-notification server
    /// connection. Absent section → `None` → the Gotify subsystem
    /// stays dormant.
    #[serde(default)]
    gotify: Option<GotifyConfig>,
}

#[derive(Debug, Deserialize)]
struct OrgEntry {
    name: String,
    /// Account `display_name`s every project in this org is allowed
    /// to spawn under. Required; cross-validated against `[[accounts]]`.
    accounts: Vec<String>,
    #[serde(default)]
    projects: Vec<ProjectEntry>,
}

#[derive(Debug, Deserialize)]
struct ProjectEntry {
    name: String,
    path: String,
    /// When `true`, the project's lead session spawns automatically
    /// at forge launch. Multiple projects can carry this; they all
    /// spawn in the background while the launchpad picker decides
    /// which one becomes the focused tab. Defaults to `false`.
    #[serde(default)]
    auto_start: bool,
    /// Static (config-defined) worker role labels to auto-spawn
    /// alongside this project's lead, each resolving to a charter + kick
    /// under `~/.claude/forge-team/<label>/`. Dynamic (LLM-spawned)
    /// workers are separate and persisted to the redb store, not listed
    /// here. Empty / missing means no static workers.
    #[serde(default)]
    static_workers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AccountEntry {
    display_name: String,
    config_dir: String,
    /// When true, sessions for this account spawn with the
    /// wire-classification rewriter proxy attached
    /// (`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` env vars stamped on
    /// the claude subprocess). When false, claude talks direct to
    /// Anthropic and the wire signals carry the CLI's native
    /// `sdk-cli` classification. Defaults to `true` so existing
    /// forge.toml files without the field behave as if they had
    /// it enabled.
    #[serde(default = "default_account_proxy")]
    proxy: bool,
}

fn default_account_proxy() -> bool {
    true
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedAccount {
    pub display_name: String,
    pub config_dir: PathBuf,
    /// Whether the wire-classification rewriter proxy should be
    /// attached to sessions for this account. See
    /// [`AccountEntry::proxy`].
    pub proxy: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedConfig {
    pub orgs: Vec<LoadedOrg>,
    pub projects: Vec<LoadedProject>,
    /// Index into `projects` for the default startup target -
    /// alphabetically-first project that carries `auto_start = true`,
    /// falling back to the alphabetically-first project overall
    /// when no project opts in. Used by the `forge` (no argv) fixture
    /// / smoke paths; the production launchpad picker overrides.
    pub default_index: usize,
    pub accounts: Vec<LoadedAccount>,
    /// `[ui]` section knobs. All fields have defaults; absent
    /// section means every field is at its default.
    pub ui: UiSettings,
    /// `[gotify]` server connection, or `None` when the section is
    /// absent (Gotify disabled).
    pub gotify: Option<GotifyConfig>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedOrg {
    pub name: String,
    /// Pinned account `display_name`s shared by all projects in
    /// this org. Always non-empty (load enforces).
    pub accounts: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedProject {
    pub name: String,
    pub path: PathBuf,
    /// Original path string from `forge.toml`, preserved for display
    /// (e.g. `~/Projects/forge` with `~` un-expanded). Use `path` for
    /// filesystem access; this for human-readable output.
    pub display_path: String,
    /// Name of the org this project belongs to (matches
    /// `LoadedOrg.name`). Workspace `project_accounts_for` resolves
    /// the pin via this back-reference.
    pub org: String,
    /// Cached pinned account list from the project's org. Duplicated
    /// here so callers don't need to walk the org list on every
    /// resolution.
    pub accounts: Vec<String>,
    /// `true` when the project should spawn automatically at forge
    /// launch.
    pub auto_start: bool,
    /// Validated static-worker labels for this project (format only -
    /// existence of the per-label charter files at
    /// `~/.claude/forge-team/<label>/{charter,kick}.md` is checked
    /// lazily at spawn time, not here). Empty means no static workers.
    /// See `crate::team::Role` + `crate::team::validate_label`.
    pub static_workers: Vec<String>,
}

impl LoadedConfig {
    pub(crate) fn default_project(&self) -> &LoadedProject {
        &self.projects[self.default_index]
    }

    /// Iterate every project that should spawn at forge launch
    /// (`auto_start = true`).
    pub(crate) fn auto_start_projects(&self) -> impl Iterator<Item = &LoadedProject> {
        self.projects.iter().filter(|p| p.auto_start)
    }

    /// Empty `LoadedConfig` for the `testing` feature's
    /// `Workspace::testing_stub`. Production code paths that need a
    /// project (e.g. `default_project`) will panic when called on
    /// this value - tests that only need `domain_handles` access
    /// never reach those paths.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            orgs: Vec::new(),
            projects: Vec::new(),
            default_index: 0,
            accounts: Vec::new(),
            ui: UiSettings::default(),
            gotify: None,
        }
    }
}

/// The subdirectory holding every file forge itself owns (config,
/// state, cron, lock), kept apart from claude's own top-level
/// config-dir files. Pure path join; call [`ensure_forge_data_dir`]
/// when the directory has to exist before writing into it.
pub(crate) fn forge_data_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("forge")
}

/// [`forge_data_dir`] with a `create_dir_all`, returning the path.
pub(crate) fn ensure_forge_data_dir(config_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = forge_data_dir(config_dir);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Read forge.toml, preferring `forge/forge.toml` and falling back to
/// the legacy top-level `forge.toml` (with a warn). forge never writes
/// forge.toml, so the fallback lets a Syncthing-synced config dir stay
/// readable until every machine is on this build and the file is moved
/// under `forge/`. Returns the path read plus its raw contents.
fn read_config(config_dir: &Path) -> Result<(PathBuf, String), WorkspaceError> {
    let preferred = forge_data_dir(config_dir).join("forge.toml");
    match fs::read_to_string(&preferred) {
        Ok(raw) => Ok((preferred, raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let legacy = config_dir.join("forge.toml");
            match fs::read_to_string(&legacy) {
                Ok(raw) => {
                    tracing::warn!(
                        target: "forge_workspace::config",
                        legacy = %legacy.display(),
                        "forge.toml read from the legacy top-level path; move it under forge/ (the top-level fallback is a rollout aid)",
                    );
                    Ok((legacy, raw))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(WorkspaceError::ConfigMissing { path: preferred })
                }
                Err(e) => Err(WorkspaceError::ConfigInvalid {
                    path: legacy,
                    message: format!("io error: {e}"),
                }),
            }
        }
        Err(e) => Err(WorkspaceError::ConfigInvalid {
            path: preferred,
            message: format!("io error: {e}"),
        }),
    }
}

/// Load + validate `forge.toml`. Returns the parsed orgs + projects
/// with `~` expanded.
pub(crate) fn load_from_dir(config_dir: &Path) -> Result<LoadedConfig, WorkspaceError> {
    let (path, raw) = read_config(config_dir)?;

    let parsed: ForgeToml = toml::from_str(&raw)
        .map_err(|source| WorkspaceError::ConfigParse { path: path.clone(), source })?;

    if parsed.orgs.is_empty() {
        return Err(WorkspaceError::NoOrgsConfigured { path });
    }
    if parsed.accounts.is_empty() {
        return Err(WorkspaceError::NoAccountsConfigured { path });
    }

    // Validate accounts first - orgs cross-reference them.
    let mut seen_account_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut accounts: Vec<LoadedAccount> = Vec::with_capacity(parsed.accounts.len());
    for entry in parsed.accounts {
        if !seen_account_names.insert(entry.display_name.clone()) {
            return Err(WorkspaceError::DuplicateAccount { path, name: entry.display_name });
        }
        accounts.push(LoadedAccount {
            display_name: entry.display_name,
            config_dir: expand_home(&entry.config_dir),
            proxy: entry.proxy,
        });
    }

    // Validate orgs + build the flat project list.
    let mut seen_org_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_project_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut orgs: Vec<LoadedOrg> = Vec::with_capacity(parsed.orgs.len());
    let mut projects: Vec<LoadedProject> = Vec::new();
    for org_entry in parsed.orgs {
        if !seen_org_names.insert(org_entry.name.clone()) {
            return Err(WorkspaceError::DuplicateOrg { path, name: org_entry.name });
        }
        if org_entry.accounts.is_empty() {
            return Err(WorkspaceError::EmptyOrgAccounts { path, org: org_entry.name });
        }
        for account in &org_entry.accounts {
            if !seen_account_names.contains(account) {
                let mut valid: Vec<&str> = seen_account_names.iter().map(String::as_str).collect();
                valid.sort_unstable();
                return Err(WorkspaceError::UnknownOrgAccount {
                    path,
                    org: org_entry.name,
                    account: account.clone(),
                    valid: valid.join(", "),
                });
            }
        }
        if org_entry.projects.is_empty() {
            return Err(WorkspaceError::EmptyOrg { path, org: org_entry.name });
        }
        for project_entry in org_entry.projects {
            if !seen_project_names.insert(project_entry.name.clone()) {
                return Err(WorkspaceError::DuplicateProject { path, name: project_entry.name });
            }
            let mut static_worker_labels: Vec<String> =
                Vec::with_capacity(project_entry.static_workers.len());
            let mut seen_labels: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for raw_label in &project_entry.static_workers {
                let label = raw_label.trim().to_owned();
                if let Err(label_err) = crate::team::validate_label(&label) {
                    return Err(WorkspaceError::UnknownStaticWorker {
                        path: path.clone(),
                        project_name: project_entry.name.clone(),
                        role: format!("{raw_label} ({label_err})"),
                    });
                }
                if !seen_labels.insert(label.clone()) {
                    return Err(WorkspaceError::DuplicateStaticWorker {
                        path: path.clone(),
                        project_name: project_entry.name.clone(),
                        role: raw_label.clone(),
                    });
                }
                static_worker_labels.push(label);
            }
            projects.push(LoadedProject {
                name: project_entry.name,
                path: expand_home(&project_entry.path),
                display_path: project_entry.path,
                org: org_entry.name.clone(),
                accounts: org_entry.accounts.clone(),
                auto_start: project_entry.auto_start,
                static_workers: static_worker_labels,
            });
        }
        orgs.push(LoadedOrg { name: org_entry.name, accounts: org_entry.accounts });
    }

    if projects.is_empty() {
        return Err(WorkspaceError::NoProjectsConfigured { path });
    }

    // Default project resolution (used by `forge` without argv when
    // the launchpad picker isn't available - fixture tests, smoke
    // tests, etc.; the production path lands on the launchpad
    // picker first):
    // 1. Alphabetically-first `auto_start = true` project.
    // 2. Else alphabetically-first project overall.
    let default_index = {
        let mut alpha: Vec<usize> = (0..projects.len()).collect();
        alpha.sort_by(|a, b| projects[*a].name.cmp(&projects[*b].name));
        alpha.iter().copied().find(|&i| projects[i].auto_start).unwrap_or_else(|| alpha[0])
    };

    Ok(LoadedConfig {
        orgs,
        projects,
        default_index,
        accounts,
        ui: parsed.ui,
        gotify: parsed.gotify,
    })
}

pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Write `forge/forge.toml` (the production location).
    fn write_config(dir: &std::path::Path, contents: &str) {
        let forge = ensure_forge_data_dir(dir).expect("forge/ dir");
        fs::write(forge.join("forge.toml"), contents).expect("write forge/forge.toml");
    }

    /// Write the legacy top-level `forge.toml` (fallback-path tests).
    fn write_legacy_config(dir: &std::path::Path, contents: &str) {
        fs::write(dir.join("forge.toml"), contents).expect("write forge.toml");
    }

    fn minimal_config() -> &'static str {
        r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#
    }

    #[test]
    fn parses_gotify_block() {
        let dir = tempdir().expect("tempdir");
        let raw = format!(
            "{}\n[gotify]\nurl = \"https://g.example\"\nclient_token = \"Cabc\"\n",
            minimal_config()
        );
        write_config(dir.path(), &raw);
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(
            config.gotify,
            Some(forge_primitives::GotifyConfig {
                url: "https://g.example".to_owned(),
                client_token: "Cabc".to_owned(),
            })
        );
    }

    #[test]
    fn absent_gotify_block_is_none() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.gotify, None);
    }

    #[test]
    fn forge_data_dir_is_the_forge_subfolder() {
        let dir = tempdir().expect("tempdir");
        assert_eq!(forge_data_dir(dir.path()), dir.path().join("forge"));
    }

    #[test]
    fn ensure_forge_data_dir_creates_the_subfolder() {
        let dir = tempdir().expect("tempdir");
        let created = ensure_forge_data_dir(dir.path()).expect("create forge/");
        assert_eq!(created, dir.path().join("forge"));
        assert!(created.is_dir(), "forge/ exists after ensure");
    }

    #[test]
    fn parses_minimal_config() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.orgs.len(), 1);
        assert_eq!(config.orgs[0].name, "Personal");
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.default_project().name, "forge");
        assert_eq!(config.default_project().org, "Personal");
        assert_eq!(config.default_project().accounts, vec!["Subspace"]);
        assert!(config.default_project().auto_start);
    }

    #[test]
    fn missing_file_returns_config_missing() {
        let dir = tempdir().expect("tempdir");
        let err = load_from_dir(dir.path()).expect_err("missing should error");
        assert!(matches!(err, WorkspaceError::ConfigMissing { .. }));
    }

    #[test]
    fn reads_forge_toml_from_forge_subfolder() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("loads from forge/");
        assert_eq!(config.default_project().name, "forge");
    }

    #[test]
    fn falls_back_to_legacy_top_level_forge_toml() {
        let dir = tempdir().expect("tempdir");
        // Only the legacy top-level file exists (no forge/forge.toml).
        write_legacy_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("loads via legacy fallback");
        assert_eq!(config.default_project().name, "forge");
    }

    #[test]
    fn prefers_forge_subfolder_over_legacy_when_both_present() {
        let dir = tempdir().expect("tempdir");
        // Legacy top-level names project "legacy"; forge/ names "forge".
        write_legacy_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "legacy"
path = "~/Projects/legacy"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("loads");
        assert_eq!(config.default_project().name, "forge", "forge/ wins over the legacy top-level");
    }

    #[test]
    fn malformed_toml_returns_config_parse() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), "not valid = = toml");
        let err = load_from_dir(dir.path()).expect_err("malformed should error");
        assert!(matches!(err, WorkspaceError::ConfigParse { .. }));
    }

    #[test]
    fn no_orgs_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("missing orgs should error");
        assert!(matches!(err, WorkspaceError::NoOrgsConfigured { .. }));
    }

    #[test]
    fn empty_org_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Empty"
accounts = ["Subspace"]

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("org without projects should error");
        assert!(matches!(err, WorkspaceError::EmptyOrg { org, .. } if org == "Empty"));
    }

    #[test]
    fn empty_org_accounts_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = []

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("empty accounts should error");
        assert!(matches!(err, WorkspaceError::EmptyOrgAccounts { org, .. } if org == "Personal"));
    }

    #[test]
    fn unknown_org_account_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace", "Bogus"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("unknown account should error");
        match err {
            WorkspaceError::UnknownOrgAccount { org, account, .. } => {
                assert_eq!(org, "Personal");
                assert_eq!(account, "Bogus");
            }
            other => panic!("expected UnknownOrgAccount, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_org_name_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "aware"
path = "~/Projects/aware"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate org should error");
        assert!(matches!(err, WorkspaceError::DuplicateOrg { name, .. } if name == "Personal"));
    }

    #[test]
    fn duplicate_project_name_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Subspace"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/subspace-forge"

[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate project should error");
        assert!(matches!(err, WorkspaceError::DuplicateProject { name, .. } if name == "forge"));
    }

    #[test]
    fn default_project_is_alpha_first_auto_start() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "zebra"
path = "~/Projects/zebra"
auto_start = true
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
auto_start = true
[[orgs.projects]]
name = "middle"
path = "~/Projects/middle"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        // Auto-start projects: alpha + zebra. Alphabetical-first: alpha.
        assert_eq!(config.default_project().name, "alpha");
    }

    #[test]
    fn default_project_falls_back_to_alpha_first_overall() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "zebra"
path = "~/Projects/zebra"
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.default_project().name, "alpha");
    }

    #[test]
    fn auto_start_projects_iterator_returns_only_opted_in() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
auto_start = true
[[orgs.projects]]
name = "beta"
path = "~/Projects/beta"
[[orgs.projects]]
name = "gamma"
path = "~/Projects/gamma"
auto_start = true
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let names: Vec<&str> = config.auto_start_projects().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "gamma"]);
    }

    #[test]
    fn missing_accounts_returns_no_accounts_configured() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("missing accounts should error");
        assert!(matches!(err, WorkspaceError::NoAccountsConfigured { .. }));
    }

    #[test]
    fn duplicate_account_display_name_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-other"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate account should error");
        assert!(matches!(err, WorkspaceError::DuplicateAccount { name, .. } if name == "Subspace"));
    }

    #[test]
    fn legacy_selection_section_is_silently_ignored() {
        let dir = tempdir().expect("tempdir");
        let mut config_text = minimal_config().to_owned();
        config_text.push_str("\n[selection]\npolicy = \"round_robin\"\n");
        write_config(dir.path(), &config_text);
        let config = load_from_dir(dir.path()).expect("legacy [selection] should be ignored");
        assert_eq!(config.default_project().name, "forge");
    }
}

#[cfg(test)]
mod static_worker_tests {
    use super::*;

    fn write_config(dir: &std::path::Path, contents: &str) {
        let forge = ensure_forge_data_dir(dir).expect("forge/ dir");
        std::fs::write(forge.join("forge.toml"), contents).expect("write forge/forge.toml");
    }

    #[test]
    fn project_without_static_workers_field_loads_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_config(
            tmp.path(),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "p1"
path = "/tmp/p1"

[[accounts]]
display_name = "acct-a"
config_dir = "/tmp/acct-a"
"#,
        );
        let cfg = load_from_dir(tmp.path()).expect("load ok");
        let p = cfg.projects.iter().find(|p| p.name == "p1").expect("p1 present");
        assert!(p.static_workers.is_empty(), "missing static_workers field -> empty");
    }

    #[test]
    fn project_with_static_workers_field_parses_labels() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_config(
            tmp.path(),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "p1"
path = "/tmp/p1"
static_workers = ["planner", "implementer", "reviewer", "debugger", "tester"]

[[accounts]]
display_name = "acct-a"
config_dir = "/tmp/acct-a"
"#,
        );
        let cfg = load_from_dir(tmp.path()).expect("load ok");
        let p = cfg.projects.iter().find(|p| p.name == "p1").expect("p1 present");
        assert_eq!(
            p.static_workers,
            vec![
                "planner".to_owned(),
                "implementer".to_owned(),
                "reviewer".to_owned(),
                "debugger".to_owned(),
                "tester".to_owned(),
            ]
        );
    }

    #[test]
    fn project_with_partial_static_workers_only_enables_listed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_config(
            tmp.path(),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "p1"
path = "/tmp/p1"
static_workers = ["reviewer", "planner"]

[[accounts]]
display_name = "acct-a"
config_dir = "/tmp/acct-a"
"#,
        );
        let cfg = load_from_dir(tmp.path()).expect("load ok");
        let p = cfg.projects.iter().find(|p| p.name == "p1").expect("p1 present");
        assert_eq!(p.static_workers, vec!["reviewer".to_owned(), "planner".to_owned()]);
    }

    #[test]
    fn arbitrary_role_label_accepted_existence_checked_lazily_at_spawn() {
        // Post-#220 the static_workers field is an open set: any
        // well-formed label is accepted at config-load. The disk-side
        // existence check fires when a worker actually spawns.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_config(
            tmp.path(),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "p1"
path = "/tmp/p1"
static_workers = ["planner", "researcher", "hub-modules/custom"]

[[accounts]]
display_name = "acct-a"
config_dir = "/tmp/acct-a"
"#,
        );
        let cfg = load_from_dir(tmp.path()).expect("open-set labels load ok");
        let p = cfg.projects.iter().find(|p| p.name == "p1").expect("p1 present");
        assert_eq!(
            p.static_workers,
            vec!["planner".to_owned(), "researcher".to_owned(), "hub-modules/custom".to_owned(),]
        );
    }

    #[test]
    fn malformed_label_rejected_at_config_load() {
        // Path-traversal-shaped labels reject loud; ditto empty / `.`
        // / leading `/`.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_config(
            tmp.path(),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "p1"
path = "/tmp/p1"
static_workers = ["planner", "../escape"]

[[accounts]]
display_name = "acct-a"
config_dir = "/tmp/acct-a"
"#,
        );
        let err = load_from_dir(tmp.path()).expect_err("must reject");
        let msg = format!("{err}");
        assert!(msg.contains("escape"), "error must name the malformed label; got: {msg}");
        assert!(msg.contains("p1"), "error must name the project; got: {msg}");
    }

    #[test]
    fn duplicate_label_in_static_workers_rejects() {
        // Only one instance per label per project. Duplicate entries
        // reject loud rather than silently dedup.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_config(
            tmp.path(),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]
[[orgs.projects]]
name = "p1"
path = "/tmp/p1"
static_workers = ["planner", "planner"]

[[accounts]]
display_name = "acct-a"
config_dir = "/tmp/acct-a"
"#,
        );
        let err = load_from_dir(tmp.path()).expect_err("must reject");
        let msg = format!("{err}");
        assert!(msg.contains("planner"), "error must name duplicate label; got: {msg}");
        assert!(msg.contains("p1"), "error must name the project; got: {msg}");
    }
}
