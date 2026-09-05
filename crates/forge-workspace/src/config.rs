//! `forge.toml` schema + loader.
//!
//! **Org model.** Projects are grouped under `[[orgs]]` (Stargate,
//! Gateway, Personal, etc.). Each org carries the `accounts = [...]`
//! pin shared by all its projects, replacing the per-project pin
//! that lived here before. Projects within an org keep a flat list
//! via `[[orgs.projects]]`. Multiple projects can carry
//! `auto_start = true`; all auto-start projects spawn at launch and
//! the first one (alphabetical) becomes the focused tab.
//!
//! **Selection policy.** A deterministic `AssignmentPlan`, computed
//! once every account reaches a terminal loading state. Its pool is the
//! org's `accounts` list in the order written there, narrowed to the
//! accounts that came up `Ready` and then to those not at their cap,
//! falling back to the capped ones only when every candidate is capped
//! so a project never goes dark. Each project takes an offset from its
//! position in the project list and a session lands on
//! `pool[(offset + session_n) % pool.len()]`. `experimental` accounts
//! are excluded from the pool entirely. Utilization is never compared
//! between accounts; it collapses to one boolean per account. A
//! round-robin cursor over the same pool is the fallback for spawns
//! that happen before the plan exists.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use forge_primitives::GotifyConfig;
use forge_primitives::account::Provider;
use forge_primitives::permission::PermissionMode;
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
    /// Optional `[dictate]` section - local dictation. Absent section
    /// -> disabled, which is what keeps a 3 GB model download opt-in.
    #[serde(default)]
    dictate: crate::dictate::DictateSettings,
    /// Optional `[gotify]` section - the inbound-notification server
    /// connection. Absent section → `None` → the Gotify subsystem
    /// stays dormant.
    #[serde(default)]
    gotify: Option<GotifyConfig>,
    /// Optional `[plugins]` section - opt-in plugin auto-update.
    /// Absent section → all defaults, which leaves auto-update off.
    #[serde(default)]
    plugins: PluginSettings,
    /// Optional top-level `[env]` table - the BASE every session
    /// starts from, overridden per key by `[accounts.env]` and then by
    /// `[projects.<name>.env]`. Merged into `LoadedAccount.env` at
    /// load; the project layer is applied at spawn. Absent -> empty.
    #[serde(default)]
    env: HashMap<String, String>,
    /// `[projects.<name>.env]` tables keyed by project name, drained
    /// into `LoadedProject.env` at load. A name no `[[orgs.projects]]`
    /// declares is a load error, not a silent no-op.
    #[serde(default)]
    projects: HashMap<String, ProjectEnvEntry>,
}

/// One `[projects.<name>.env]` table. Unknown fields are rejected so a
/// mistyped inner table (`envs`) or keys written without the `.env`
/// nesting fail loudly instead of loading as an empty env.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectEnvEntry {
    #[serde(default)]
    env: HashMap<String, String>,
    /// Path to a `KEY=value` file whose entries join this project's env,
    /// so a secret can live outside forge.toml. Read once at load, like
    /// every other value here, so rotating it needs a forge restart.
    #[serde(default)]
    env_file: Option<String>,
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
    /// spawn in the background, and from the launchpad none of them
    /// is focused until the user picks one. Defaults to `false`.
    #[serde(default)]
    auto_start: bool,
}

/// Unknown fields are rejected so a near-miss key (`providers`) fails
/// loudly instead of loading and leaving the account probing the wrong
/// endpoint until preflight hangs on it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountEntry {
    display_name: String,
    config_dir: String,
    /// Which backend this account talks to. Required: an account that
    /// does not say probes the wrong endpoint and bails at preflight,
    /// so silence is the dangerous answer. Held as `Option` only so the
    /// absent case can name the account; `None` is a load error.
    provider: Option<forge_primitives::account::Provider>,
    /// Free-form environment stamped onto the account's `claude`
    /// subprocess at spawn. Absent `[accounts.env]` table -> empty.
    /// A base-url provider reads its `ANTHROPIC_BASE_URL` and
    /// `ANTHROPIC_AUTH_TOKEN` from here.
    #[serde(default)]
    env: HashMap<String, String>,
    /// When true, the account is excluded from every auto-assignment
    /// path (assignment plan + round-robin fallback) but stays
    /// globally selectable in the `/account` picker. Defaults to
    /// false so existing accounts keep rotating normally.
    #[serde(default)]
    experimental: bool,
    /// Optional CLI permission mode stamped onto every session this
    /// account spawns, overriding the launcher's session default;
    /// validated against `PermissionMode::from_wire` at load. The
    /// account owns the credential and endpoint, so it owns the mode.
    #[serde(default)]
    permission_mode: Option<String>,
}

#[derive(Debug)]
pub(crate) struct LoadedAccount {
    pub display_name: String,
    pub config_dir: PathBuf,
    /// Declared backend. Drives the usage probe and the billing shape.
    /// See [`AccountEntry::provider`].
    pub provider: forge_primitives::account::Provider,
    /// Per-account environment from `[accounts.env]`, stamped onto the
    /// spawned `claude` subprocess. See [`AccountEntry::env`].
    pub env: HashMap<String, String>,
    /// Excluded from auto-assignment, picker-only. See
    /// [`AccountEntry::experimental`].
    pub experimental: bool,
    /// Optional CLI permission mode stamped into launch settings at
    /// spawn. See [`AccountEntry::permission_mode`].
    pub permission_mode: Option<PermissionMode>,
}

/// The `[plugins]` section. Unknown fields are rejected so a mistyped
/// key cannot silently leave auto-update doing nothing.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PluginSettings {
    /// Update every installed plugin once at forge boot - the switch
    /// alone governs, there is no per-marketplace or per-plugin opt-out.
    /// Off by default: an auto-applied plugin update can break a
    /// load-bearing session mid-day, so forge only ever moves a plugin
    /// the user opted in.
    #[serde(default)]
    pub auto_update: bool,
}

#[derive(Debug)]
pub(crate) struct LoadedConfig {
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
    /// `[dictate]` section knobs. Absent section means dictation is
    /// off and preflight skips it entirely.
    pub dictate: crate::dictate::DictateSettings,
    /// `[gotify]` server connection, or `None` when the section is
    /// absent (Gotify disabled).
    pub gotify: Option<GotifyConfig>,
    /// `[plugins]` section knobs. Absent section means auto-update is
    /// off.
    pub plugins: PluginSettings,
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
    /// Per-project environment from `[projects.<name>.env]`, layered
    /// over the account's env at spawn. An `ANTHROPIC_BASE_URL` or
    /// `ANTHROPIC_AUTH_TOKEN` here desyncs forge's own accounting -
    /// usage probe, plan detection and the picker all read the ACCOUNT
    /// map, so they measure a different endpoint.
    pub env: HashMap<String, String>,
}

/// Complete `[env]` < `[accounts.env]` < `[projects.<name>.env]`,
/// narrowest winning per key, over the already-merged `account_env`.
/// Applied here rather than at load because one account serves many
/// projects, so merging earlier would leak a project's keys into every
/// other project on that account.
pub(crate) fn session_env(
    project: &LoadedProject,
    account_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env = account_env.clone();
    env.extend(project.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

/// Sorted key NAMES a project contributes - never values; these tables
/// hold tokens.
pub(crate) fn applied_env_keys(project: &LoadedProject) -> String {
    let mut keys: Vec<&str> = project.env.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys.join(", ")
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
            projects: Vec::new(),
            default_index: 0,
            accounts: Vec::new(),
            ui: UiSettings::default(),
            dictate: crate::dictate::DictateSettings::default(),
            gotify: None,
            plugins: PluginSettings::default(),
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

/// Read `forge/forge.toml`. Returns the path read plus its raw
/// contents.
fn read_config(config_dir: &Path) -> Result<(PathBuf, String), WorkspaceError> {
    let path = forge_data_dir(config_dir).join("forge.toml");
    match fs::read_to_string(&path) {
        Ok(raw) => Ok((path, raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(WorkspaceError::ConfigMissing { path })
        }
        Err(e) => Err(WorkspaceError::ConfigInvalid { path, message: format!("io error: {e}") }),
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

    // Global `[env]` is the BASE each account's effective env starts
    // from; the account's own `[accounts.env]` extends it, so account
    // keys override global keys.
    let global_env = parsed.env;

    // Drained per project as the org loop builds the project list;
    // whatever is left over named no declared project.
    let mut project_env_tables = parsed.projects;

    // Validate accounts first - orgs cross-reference them.
    let mut seen_account_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Collected rather than reported one at a time: a first run after
    // the key became required trips every account at once, and naming
    // one per boot is that many edit-and-restart cycles.
    let missing_provider: Vec<String> = parsed
        .accounts
        .iter()
        .filter(|entry| entry.provider.is_none())
        .map(|entry| entry.display_name.clone())
        .collect();
    if !missing_provider.is_empty() {
        return Err(WorkspaceError::AccountsMissingProvider { path, names: missing_provider });
    }

    let mut accounts: Vec<LoadedAccount> = Vec::with_capacity(parsed.accounts.len());
    for entry in parsed.accounts {
        if !seen_account_names.insert(entry.display_name.clone()) {
            return Err(WorkspaceError::DuplicateAccount { path, name: entry.display_name });
        }
        let Some(provider) = entry.provider else {
            return Err(WorkspaceError::AccountsMissingProvider {
                path,
                names: vec![entry.display_name],
            });
        };
        let permission_mode = match entry.permission_mode.as_deref() {
            None => None,
            Some(raw) => match PermissionMode::from_wire(raw) {
                Some(mode) => Some(mode),
                None => {
                    return Err(WorkspaceError::AccountInvalidPermissionMode {
                        path,
                        name: entry.display_name.clone(),
                        value: raw.to_owned(),
                    });
                }
            },
        };
        let mut env = global_env.clone();
        env.extend(entry.env);
        trim_setup_token(&mut env);
        // A base-url provider probes `{ANTHROPIC_BASE_URL}/...`, so an
        // absent key would leave the probe pointed at Anthropic's host
        // with the wrong bearer. Refuse at load rather than at preflight.
        let base_url = env.get("ANTHROPIC_BASE_URL").map(|v| v.trim()).filter(|v| !v.is_empty());
        if provider.uses_base_url() && base_url.is_none() {
            return Err(WorkspaceError::AccountProviderNeedsBaseUrl {
                path,
                name: entry.display_name,
            });
        }
        // The key probe is `{base}/v1/key`, so a base that is the bare
        // host resolves to `openrouter.ai/v1/key` - which answers 200
        // with a marketing page. 200 is the one status the probe reads
        // as success, so it would reach the decode arm, retry to the
        // iteration cap and bail the account, stopping forge from
        // starting. Refuse the base here, where the message can say so.
        if provider == Provider::Openrouter
            && !base_url.is_some_and(|v| v.trim_end_matches('/').ends_with("/api"))
        {
            return Err(WorkspaceError::OpenrouterBaseUrlNotApiRoot {
                path,
                name: entry.display_name,
            });
        }
        // Legal but self-inconsistent: the env is still stamped on the
        // spawned session, so chat goes to the proxy while usage probes
        // the keychain. Before `provider` existed the combination could
        // not be expressed, so warn rather than refuse.
        if provider == Provider::Anthropic && base_url.is_some() {
            tracing::warn!(
                target: "forge_workspace::config",
                account = %entry.display_name,
                "account sets provider = \"anthropic\" beside an ANTHROPIC_BASE_URL; sessions \
                 will use that endpoint while usage probes the keychain",
            );
        }
        accounts.push(LoadedAccount {
            display_name: entry.display_name,
            config_dir: expand_home(&entry.config_dir),
            provider,
            env,
            experimental: entry.experimental,
            permission_mode,
        });
    }

    // Experimental account names, for the "org lists only experimental
    // accounts" validation below.
    let experimental_account_names: std::collections::HashSet<String> =
        accounts.iter().filter(|a| a.experimental).map(|a| a.display_name.clone()).collect();

    // Validate orgs + build the flat project list.
    let mut seen_org_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_project_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
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
        // An org whose accounts are all experimental leaves its projects
        // with nothing assignable - the auto_start path would fall
        // through to a foreign non-experimental account. Reject at load.
        if org_entry.accounts.iter().all(|a| experimental_account_names.contains(a)) {
            return Err(WorkspaceError::AllExperimentalOrgAccounts { path, org: org_entry.name });
        }
        if org_entry.projects.is_empty() {
            return Err(WorkspaceError::EmptyOrg { path, org: org_entry.name });
        }
        for project_entry in org_entry.projects {
            if !seen_project_names.insert(project_entry.name.clone()) {
                return Err(WorkspaceError::DuplicateProject { path, name: project_entry.name });
            }
            let project_env = project_env_tables
                .remove(&project_entry.name)
                .map(|table| resolve_project_env(&project_entry.name, table))
                .unwrap_or_default();
            projects.push(LoadedProject {
                name: project_entry.name,
                path: expand_home(&project_entry.path),
                display_path: project_entry.path,
                org: org_entry.name.clone(),
                accounts: org_entry.accounts.clone(),
                auto_start: project_entry.auto_start,
                env: project_env,
            });
        }
    }

    // A `[projects.<name>.env]` table repeats a project name by hand,
    // so a typo lands nowhere. Same treatment as an org naming an
    // undeclared account: refuse to boot and list the valid names.
    let mut unknown_env_projects: Vec<&str> =
        project_env_tables.keys().map(String::as_str).collect();
    unknown_env_projects.sort_unstable();
    if !unknown_env_projects.is_empty() {
        let mut valid: Vec<&str> = seen_project_names.iter().map(String::as_str).collect();
        valid.sort_unstable();
        return Err(WorkspaceError::UnknownProjectEnv {
            projects: unknown_env_projects.join(", "),
            valid: valid.join(", "),
            path,
        });
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
        projects,
        default_index,
        accounts,
        ui: parsed.ui,
        dictate: parsed.dictate,
        gotify: parsed.gotify,
        plugins: parsed.plugins,
    })
}

/// A project's env: the `env_file` entries with the inline `env` table
/// layered over them, since the inline form is the more explicit
/// statement of the two.
/// Trim the setup token once here: the probe and the spawned child
/// both read these maps verbatim, so a padded value would authenticate
/// one and fail the other.
fn trim_setup_token<S: std::hash::BuildHasher>(env: &mut HashMap<String, String, S>) {
    if let Some(token) = env.get_mut(forge_providers::CLAUDE_CODE_OAUTH_TOKEN_ENV) {
        *token = token.trim().to_owned();
    }
}

fn resolve_project_env(project: &str, entry: ProjectEnvEntry) -> HashMap<String, String> {
    let mut env = entry.env_file.map(|path| read_env_file(project, &path)).unwrap_or_default();
    env.extend(entry.env);
    trim_setup_token(&mut env);
    env
}

/// Parse `KEY=value` lines, skipping blanks and `#` comments. Every way
/// this can go wrong warns and yields the keys it did read - a project's
/// env file is not worth refusing to boot over, and a partial read is
/// visible in the per-spawn applied record's key list.
fn read_env_file(project: &str, raw_path: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let skipped = |reason: &str, detail: &str| {
        tracing::warn!(
            target: "forge_workspace::config",
            event_name = "project_env_file_skipped",
            project,
            path = raw_path,
            reason,
            detail,
            "a [projects.<name>.env_file] entry did not fully apply",
        );
    };

    let path = expand_home(raw_path);
    if !path.is_absolute() {
        // A relative path resolves against the process working
        // directory, so the same config would read a different file per
        // launch directory (HR#14). Skipping is failing the operation;
        // what the rule forbids is substituting a cwd-derived answer.
        skipped("not-absolute", "use an absolute or ~/ path");
        return env;
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            let reason =
                if err.kind() == std::io::ErrorKind::NotFound { "missing" } else { "unreadable" };
            skipped(reason, &err.to_string());
            return env;
        }
    };
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some((key, value)) if !key.trim().is_empty() => {
                // Strip one matching pair of surrounding quotes, as
                // dotenv and direnv do. TOML forces quoting, so a value
                // moved out of `[projects.<name>.env]` arrives with
                // them, and keeping them yields a token silently longer
                // than intended that fails far from the cause.
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|inner| inner.strip_suffix('"'))
                    .or_else(|| value.strip_prefix('\'').and_then(|inner| inner.strip_suffix('\'')))
                    .unwrap_or(value);
                env.insert(key.trim().to_owned(), value.to_owned());
            }
            _ => skipped("malformed-line", &format!("line {}", number + 1)),
        }
    }
    env
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

    /// Resolve a project by name for the `session_env` calls below.
    fn named<'a>(config: &'a LoadedConfig, name: &str) -> &'a LoadedProject {
        config.projects.iter().find(|p| p.name == name).expect("declared project")
    }

    /// Write `forge/forge.toml` (the production location).
    fn write_config(dir: &std::path::Path, contents: &str) {
        let forge = ensure_forge_data_dir(dir).expect("forge/ dir");
        fs::write(forge.join("forge.toml"), contents).expect("write forge/forge.toml");
    }

    fn minimal_config() -> &'static str {
        r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
"#
    }

    #[test]
    fn parses_account_env_table() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "codex"
[accounts.env]
ANTHROPIC_BASE_URL = "http://localhost:18765"
ANTHROPIC_AUTH_TOKEN = "unused"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        assert_eq!(account.display_name, "Codex");
        assert_eq!(
            account.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://localhost:18765"),
        );
        assert_eq!(account.env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str), Some("unused"));
    }

    #[test]
    fn account_without_provider_fails_the_load_naming_the_account() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-no-provider"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("absent provider must not load");
        let message = err.to_string();
        assert!(
            message.contains("Stargate"),
            "the error has to name the offending account, got: {message}",
        );
        assert!(
            message.contains("anthropic") && message.contains("codex"),
            "the error has to list the accepted providers, got: {message}",
        );
    }

    #[test]
    fn codex_provider_without_a_base_url_fails_the_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex-no-base"
provider = "codex"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("codex without a base url must not load");
        let message = err.to_string();
        assert!(
            message.contains("Codex") && message.contains("ANTHROPIC_BASE_URL"),
            "the error has to name the account and the missing key, got: {message}",
        );
    }

    /// `https://openrouter.ai/v1/key` answers 200 with a marketing page,
    /// and 200 is the one status the probe treats as success, so a bare
    /// host reaches the decode arm, retries twelve times and bails the
    /// account - which stops forge starting. Catch the base at load
    /// instead, where the user can act on it.
    #[test]
    fn openrouter_base_url_without_the_api_suffix_fails_the_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Router"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Router"
config_dir = "~/.claude-router-bare-host"
provider = "openrouter"
[accounts.env]
ANTHROPIC_BASE_URL = "https://openrouter.ai"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("a bare host must not load");
        let message = err.to_string();
        assert!(
            message.contains("Router"),
            "the error has to name the offending account, got: {message}",
        );
        assert!(
            message.contains("/api"),
            "the error has to say what the base url is expected to end in, got: {message}",
        );
    }

    #[test]
    fn openrouter_base_url_with_the_api_suffix_loads() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Router"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Router"
config_dir = "~/.claude-router-ok"
provider = "openrouter"
[accounts.env]
ANTHROPIC_BASE_URL = "https://openrouter.ai/api/"
"#,
        );
        load_from_dir(dir.path()).expect("a trailing slash after /api is still the api base");
    }

    #[test]
    fn account_rejects_an_unknown_key() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-unknown-key"
provider = "anthropic"
providers = "anthropic"
"#,
        );
        // A near-miss key is the failure `provider` exists to prevent:
        // without the reject it loads, probes the wrong endpoint and
        // hangs preflight.
        let err = load_from_dir(dir.path()).expect_err("a mistyped account key must not load");
        let message = err.to_string();
        assert!(
            message.contains("providers"),
            "the error has to name the offending key, got: {message}",
        );
    }

    #[test]
    fn a_whitespace_only_base_url_is_not_a_base_url() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex-blank-base"
provider = "codex"
[accounts.env]
ANTHROPIC_BASE_URL = "   "
"#,
        );
        let err =
            load_from_dir(dir.path()).expect_err("a blank base url must not satisfy the check");
        assert!(err.to_string().contains("ANTHROPIC_BASE_URL"), "got: {err}");
    }

    #[test]
    fn account_without_env_table_is_empty() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        assert!(account.env.is_empty(), "no [accounts.env] -> empty map");
        assert!(!account.experimental, "absent experimental field defaults to false");
    }

    #[test]
    fn account_permission_mode_parses_into_the_loaded_account() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Openrouter"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Openrouter"
config_dir = "~/.claude-openrouter"
provider = "openrouter"
permission_mode = "bypassPermissions"
[accounts.env]
ANTHROPIC_BASE_URL = "https://openrouter.ai/api"
ANTHROPIC_AUTH_TOKEN = "unused"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(
            config.accounts[0].permission_mode,
            Some(PermissionMode::BypassPermissions),
            "a valid mode lands on the LoadedAccount verbatim",
        );
    }

    #[test]
    fn account_without_permission_mode_loads_with_none() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("absent key must not block the load");
        assert_eq!(
            config.accounts[0].permission_mode, None,
            "accounts without the key keep every spawn unchanged",
        );
    }

    #[test]
    fn account_with_invalid_permission_mode_fails_naming_the_accepted_set() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Openrouter"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Openrouter"
config_dir = "~/.claude-openrouter-bad"
provider = "openrouter"
permission_mode = "yolo"
[accounts.env]
ANTHROPIC_BASE_URL = "https://openrouter.ai/api"
ANTHROPIC_AUTH_TOKEN = "unused"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("an invalid mode must not load");
        let message = err.to_string();
        assert!(
            message.contains("yolo") && message.contains("Openrouter"),
            "the error has to name the offending value and account, got: {message}",
        );
        assert!(
            message.contains("bypassPermissions") && message.contains("acceptEdits"),
            "the error has to list the accepted values, got: {message}",
        );
    }

    #[test]
    fn mistyped_permission_mode_key_is_still_rejected() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-mistyped-mode"
provider = "anthropic"
permissionmode = "bypassPermissions"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("a mistyped account key must not load");
        assert!(
            err.to_string().contains("permissionmode"),
            "deny_unknown_fields has to catch the near-miss, got: {err}",
        );
    }

    #[test]
    fn parses_top_level_env_table() {
        let raw =
            format!("[env]\nCLAUDE_CODE_AUTO_COMPACT_WINDOW = \"950000\"\n{}", minimal_config());
        let parsed: ForgeToml = toml::from_str(&raw).expect("parse top-level [env]");
        assert_eq!(
            parsed.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str),
            Some("950000"),
        );
    }

    #[test]
    fn absent_top_level_env_table_is_empty() {
        let parsed: ForgeToml = toml::from_str(minimal_config()).expect("parse without [env]");
        assert!(parsed.env.is_empty(), "no [env] -> empty map");
    }

    #[test]
    fn global_env_lands_in_account_without_its_own_env() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[env]
CLAUDE_CODE_AUTO_COMPACT_WINDOW = "950000"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        assert_eq!(
            account.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str),
            Some("950000"),
            "global [env] key merged into an account with no [accounts.env]",
        );
    }

    #[test]
    fn account_env_overrides_global_env_per_key() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[env]
CLAUDE_CODE_AUTO_COMPACT_WINDOW = "950000"
[[orgs]]
name = "Personal"
accounts = ["Codex", "Gateway"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "anthropic"
[accounts.env]
CLAUDE_CODE_AUTO_COMPACT_WINDOW = "372000"
[[accounts]]
display_name = "Gateway"
config_dir = "~/.claude"
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let codex = config.accounts.iter().find(|a| a.display_name == "Codex").expect("Codex");
        let gateway =
            config.accounts.iter().find(|a| a.display_name == "Gateway").expect("Gateway");
        assert_eq!(
            codex.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str),
            Some("372000"),
            "per-account [accounts.env] overrides the global [env] key",
        );
        assert_eq!(
            gateway.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str),
            Some("950000"),
            "an account with no override inherits the global [env] key",
        );
    }

    #[test]
    fn whitespace_padded_setup_token_is_trimmed_once_at_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
[accounts.env]
CLAUDE_CODE_OAUTH_TOKEN = "  sk-ant-oat01-stargate  "
[projects.forge.env]
CLAUDE_CODE_OAUTH_TOKEN = "  sk-ant-oat01-project  "
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        // The probe reads this map through `token_bearer` and the spawn
        // path stamps it onto the child verbatim; both must see the
        // same credential.
        assert_eq!(
            account.env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some("sk-ant-oat01-stargate"),
            "the setup token is trimmed where it enters the config",
        );
        assert_eq!(
            forge_providers::token_bearer(&account.env),
            Some("sk-ant-oat01-stargate"),
            "the probe reads the same trimmed credential the child gets",
        );
        assert_eq!(
            config.projects[0].env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some("sk-ant-oat01-project"),
            "the project env layer is trimmed at load too - it reaches the child unmerged",
        );
    }

    #[test]
    fn no_global_env_leaves_account_env_untouched() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "codex"
[accounts.env]
ANTHROPIC_BASE_URL = "http://localhost:18765"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        assert_eq!(account.env.len(), 1, "no [env] -> env is exactly the account's own");
        assert_eq!(
            account.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://localhost:18765"),
        );
    }

    /// One key per precedence boundary, so a test can assert a single
    /// boundary and a future reordering of the merge fails on exactly
    /// the boundary it broke.
    fn precedence_config() -> &'static str {
        r#"
[env]
ALL_THREE = "global"
GLOBAL_PROJECT = "global"
GLOBAL_ONLY = "global"

[[orgs]]
name = "Personal"
accounts = ["Codex"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "anthropic"
[accounts.env]
ALL_THREE = "account"

[projects.forge.env]
ALL_THREE = "project"
GLOBAL_PROJECT = "project"
PROJECT_ONLY = "project"
"#
    }

    /// `session_env` for the fixture's single project + account.
    fn precedence_env() -> HashMap<String, String> {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), precedence_config());
        let config = load_from_dir(dir.path()).expect("precedence fixture loads");
        let account = &config.accounts[0];
        session_env(named(&config, "forge"), &account.env)
    }

    /// Every near-miss shape for declaring project env. Each loads
    /// clean and applies nothing without the `deny_unknown_fields`
    /// attributes; the point of the table is that they are all one
    /// class rather than five separate bugs.
    #[test]
    fn near_miss_env_declarations_are_rejected() {
        // (label, stanza appended to the base config, text the error must name)
        let cases = [("mistyped inner table", "[projects.forge.envs]\nK = \"v\"\n", "envs")];
        for (label, stanza, needle) in cases {
            let dir = tempdir().expect("tempdir");
            write_config(dir.path(), &format!("{}\n{stanza}", minimal_config()));
            let msg = load_from_dir(dir.path()).expect_err(label).to_string();
            assert!(msg.contains(needle), "{label}: error must name `{needle}`, got: {msg}");
        }
    }

    /// Write an env file and a config pointing at it. Returns the dir
    /// so it outlives the load.
    fn config_with_env_file(contents: &str, inline: &str) -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("secrets.env");
        fs::write(&file, contents).expect("write env file");
        write_config(
            dir.path(),
            &format!(
                r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"

[projects.forge]
env_file = "{file}"
{inline}
"#,
                file = file.display()
            ),
        );
        dir
    }

    #[test]
    fn env_file_entries_join_the_project_env() {
        let dir = config_with_env_file(
            "# a comment\n\nAIRMAIL_TOKEN = tok-from-file\nQUOTED = \"in-quotes\"\n\
             SINGLE = 'in-singles'\nUNMATCHED = \"dangling\n",
            "",
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let env = &config.projects[0].env;
        assert_eq!(
            env.get("AIRMAIL_TOKEN").map(String::as_str),
            Some("tok-from-file"),
            "comments and blank lines skipped, the key lands",
        );
        // Quoted is the shape a value moved out of forge.toml arrives in.
        assert_eq!(env.get("QUOTED").map(String::as_str), Some("in-quotes"));
        assert_eq!(env.get("SINGLE").map(String::as_str), Some("in-singles"));
        assert_eq!(
            env.get("UNMATCHED").map(String::as_str),
            Some("\"dangling"),
            "an unmatched quote is part of the value, not a delimiter",
        );
    }

    #[test]
    fn the_inline_table_wins_over_the_env_file_per_key() {
        let dir = config_with_env_file(
            "SHARED = from-file\nFILE_ONLY = from-file\n",
            "[projects.forge.env]\nSHARED = \"from-inline\"",
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let env = &config.projects[0].env;
        assert_eq!(env.get("SHARED").map(String::as_str), Some("from-inline"));
        assert_eq!(env.get("FILE_ONLY").map(String::as_str), Some("from-file"));
    }

    #[test]
    fn a_missing_env_file_leaves_the_project_env_empty_and_still_loads() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            &format!(
                r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"

[projects.forge]
env_file = "{}/nope.env"
"#,
                dir.path().display()
            ),
        );
        let config = load_from_dir(dir.path()).expect("a missing env file must not fail the load");
        assert!(config.projects[0].env.is_empty(), "and contributes no keys");
    }

    #[test]
    fn a_malformed_line_is_skipped_and_the_rest_applies() {
        let dir = config_with_env_file("GOOD = yes\nthis line has no equals\nALSO = fine\n", "");
        let config = load_from_dir(dir.path()).expect("happy path");
        let env = &config.projects[0].env;
        assert_eq!(env.get("GOOD").map(String::as_str), Some("yes"));
        assert_eq!(env.get("ALSO").map(String::as_str), Some("fine"));
        assert_eq!(env.len(), 2, "only the malformed line is dropped: {env:?}");
    }

    #[test]
    fn parses_project_env_table() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"

[projects.forge.env]
AIRMAIL_MCP_URL = "https://mail.example/mcp"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let project = &config.projects[0];
        assert_eq!(
            project.env.get("AIRMAIL_MCP_URL").map(String::as_str),
            Some("https://mail.example/mcp"),
            "[projects.<name>.env] lands on the named project",
        );
    }

    /// An env block naming a project that no `[[orgs.projects]]`
    /// declares is the same typo class as an org naming an undeclared
    /// account, which `load_from_dir` already refuses to boot on. A
    /// silently-ignored env block is the failure mode #551 exists to
    /// kill, so it must not load.
    /// Project names here deliberately avoid `forge`: the message
    /// contains the literal `forge.toml` and the interpolated path is
    /// `<tempdir>/forge/forge.toml`, so asserting on `forge` would
    /// pass with the valid-names listing dropped entirely.
    #[test]
    fn env_table_for_undeclared_project_is_rejected() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
[[orgs.projects]]
name = "beta"
path = "~/Projects/beta"
[[orgs.projects]]
name = "kappa"
path = "~/Projects/kappa"
[[orgs.projects]]
name = "omega"
path = "~/Projects/omega"
[[orgs.projects]]
name = "sigma"
path = "~/Projects/sigma"
[[orgs.projects]]
name = "theta"
path = "~/Projects/theta"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"

[projects.gamma.env]
AIRMAIL_TOKEN = "typo-in-the-project-name"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("undeclared project name must not load");
        let msg = err.to_string();
        assert!(msg.contains("gamma"), "error names the offending project name, got: {msg}");
        assert!(!msg.contains("delta"), "control: only the declared typo appears");
        // Sortedness against the listing's own sorted form, over six
        // names: a two-name literal passed half the time with the sort
        // dropped, because `seen_project_names` is a HashSet.
        let listed: Vec<&str> =
            msg.split("valid projects: ").nth(1).expect("valid listing").split(", ").collect();
        let mut sorted = listed.clone();
        sorted.sort_unstable();
        assert_eq!(listed, sorted, "the valid-name listing is sorted, got: {msg}");
        assert_eq!(listed.len(), 6, "every declared project is listed, got: {msg}");
    }

    /// One assertion per precedence boundary, so a reordering fails on
    /// the boundary it broke rather than on a single opaque test.
    #[test]
    fn project_env_wins_each_precedence_boundary() {
        let env = precedence_env();
        assert_eq!(
            env.get("ALL_THREE").map(String::as_str),
            Some("project"),
            "a key in all three layers resolves to the project value",
        );
        assert_eq!(
            env.get("GLOBAL_PROJECT").map(String::as_str),
            Some("project"),
            "global + project, account silent -> project wins",
        );
        assert_eq!(
            env.get("PROJECT_ONLY").map(String::as_str),
            Some("project"),
            "a project-only key reaches the session",
        );
        assert_eq!(
            env.get("GLOBAL_ONLY").map(String::as_str),
            Some("global"),
            "and the global layer still arrives, so the above are overrides not absences",
        );
    }

    /// Two projects on one account, one declaring env. The other must
    /// not receive it.
    #[test]
    fn one_projects_env_does_not_reach_another_on_the_same_account() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[orgs.projects]]
name = "airmail"
path = "~/Projects/airmail"

[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "codex"
[accounts.env]
ANTHROPIC_BASE_URL = "http://localhost:18765"

[projects.forge.env]
AIRMAIL_TOKEN = "forge-only-secret"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];

        let forge_env = session_env(named(&config, "forge"), &account.env);
        assert_eq!(
            forge_env.get("AIRMAIL_TOKEN").map(String::as_str),
            Some("forge-only-secret"),
            "the declaring project gets its own key",
        );

        let airmail_env = session_env(named(&config, "airmail"), &account.env);
        assert!(
            !airmail_env.contains_key("AIRMAIL_TOKEN"),
            "another project on the SAME account must not receive it, got: {airmail_env:?}",
        );
        assert_eq!(
            airmail_env, account.env,
            "a project declaring no env gets exactly the account env, nothing borrowed",
        );
    }

    #[test]
    fn parses_account_experimental_flag() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex", "Gateway"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "anthropic"
experimental = true
[[accounts]]
display_name = "Gateway"
config_dir = "~/.claude"
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let codex = config.accounts.iter().find(|a| a.display_name == "Codex").expect("Codex");
        let gateway =
            config.accounts.iter().find(|a| a.display_name == "Gateway").expect("Gateway");
        assert!(codex.experimental, "experimental = true parsed");
        assert!(!gateway.experimental, "account without the field defaults to false");
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

    /// The `[dictate]` plumbing leg, which the section's own serde tests
    /// cannot reach: a `parsed.dictate` never threaded into
    /// `LoadedConfig` leaves dictation off however the user writes
    /// forge.toml, and every one of those tests still passes.
    #[test]
    fn the_dictate_section_reaches_the_loaded_config() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            &format!("{}\n[dictate]\nenabled = true\nlanguage = \"en\"\n", minimal_config()),
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert!(config.dictate.enabled, "an explicit `enabled = true` must survive the load");
        assert_eq!(config.dictate.language.as_deref(), Some("en"));

        let bare = tempdir().expect("tempdir");
        write_config(bare.path(), minimal_config());
        let config = load_from_dir(bare.path()).expect("happy path");
        assert!(!config.dictate.enabled, "an absent section must leave dictation off");
    }

    #[test]
    fn absent_gotify_block_is_none() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.gotify, None);
    }

    #[test]
    fn the_plugins_section_reaches_the_loaded_config() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), &format!("{}\n[plugins]\nauto_update = true\n", minimal_config()));
        let config = load_from_dir(dir.path()).expect("happy path");
        assert!(config.plugins.auto_update);
    }

    #[test]
    fn a_stale_plugins_key_fails_the_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            &format!(
                "{}\n[plugins]\nauto_update = true\ntrusted_marketplaces = \
                 [\"claude-plugins-official\"]\n",
                minimal_config()
            ),
        );
        assert!(
            load_from_dir(dir.path()).is_err(),
            "removed keys are rejected, not silently ignored"
        );
    }

    #[test]
    fn absent_plugins_section_auto_update_is_off() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        assert!(!config.plugins.auto_update);
    }

    #[test]
    fn an_unknown_plugins_key_fails_the_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            &format!("{}\n[plugins]\ntrust_markets = [\"x\"]\n", minimal_config()),
        );
        let error = load_from_dir(dir.path()).expect_err("unknown key must fail loudly");
        assert!(error.to_string().contains("trust_markets"), "names the key: {error}");
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
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.default_project().name, "forge");
        assert_eq!(config.default_project().org, "Personal");
        assert_eq!(config.default_project().accounts, vec!["Stargate"]);
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
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
accounts = ["Stargate"]

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
accounts = ["Stargate", "Bogus"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
    fn all_experimental_org_accounts_errors() {
        // An org whose entire account list is experimental would leave
        // its auto_start project with no assignable account and silently
        // bind to a foreign non-experimental account at runtime. Reject
        // it at load.
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "anthropic"
experimental = true
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("all-experimental org should error");
        assert!(
            matches!(err, WorkspaceError::AllExperimentalOrgAccounts { ref org, .. } if org == "Personal"),
            "got {err:?}",
        );
    }

    #[test]
    fn org_with_a_non_experimental_account_loads() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Codex", "Gateway"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Codex"
config_dir = "~/.claude-codex"
provider = "anthropic"
experimental = true

[[accounts]]
display_name = "Gateway"
config_dir = "~/.claude"
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("org with a non-experimental account loads");
        assert_eq!(
            config.default_project().accounts,
            vec!["Codex".to_owned(), "Gateway".to_owned()]
        );
    }

    #[test]
    fn duplicate_org_name_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "aware"
path = "~/Projects/aware"

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
name = "Stargate"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/stargate-forge"

[[orgs]]
name = "Personal"
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
accounts = ["Stargate"]
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
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
accounts = ["Stargate"]
[[orgs.projects]]
name = "zebra"
path = "~/Projects/zebra"
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
accounts = ["Stargate"]
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
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
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
accounts = ["Stargate"]
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
accounts = ["Stargate"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-other"
provider = "anthropic"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate account should error");
        assert!(matches!(err, WorkspaceError::DuplicateAccount { name, .. } if name == "Stargate"));
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
