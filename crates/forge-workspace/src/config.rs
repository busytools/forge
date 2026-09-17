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
//! **Selection policy.** The gateway's declared-model walk: over the
//! org's `accounts` primaries and `fallback_accounts`, the first
//! account that declares the project's `model`, preferring ready over
//! saturated and keeping bailed last (saturated then leads to a
//! cooling filter). Utilization is never compared between accounts; it
//! collapses to one boolean per account. Every spawn in an org takes
//! the same walk, so there is no per-session spread.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_primitives::GotifyConfig;
use forge_primitives::account::Provider;
use forge_primitives::permission::PermissionMode;
use forge_primitives::slack::SlackConfig;
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
    /// Optional `[[slack]]` sections - one entry per workspace. Absent or
    /// empty → the connector stays dormant.
    #[serde(default)]
    slack: Vec<SlackConfig>,
    /// Optional `[plugins]` section - opt-in plugin auto-update.
    /// Absent section → all defaults, which leaves auto-update off.
    #[serde(default)]
    plugins: PluginSettings,
    /// Optional `[gateway]` section - the inference listener's port.
    #[serde(default)]
    gateway: Option<GatewaySettings>,
    /// Ghost of the deleted `[workers]` section: read only so a stale
    /// synced forge.toml still carrying it warns at load instead of
    /// sitting there silently ignored.
    #[serde(default)]
    workers: Option<toml::Value>,
    /// Ghost of the deleted `[projects.<name>]` side tables: read only
    /// so a pre-colocation forge.toml still carrying them warns at load
    /// instead of silently dropping every per-project key.
    #[serde(default)]
    projects: Option<toml::Value>,
    /// Optional top-level `[env]` table - the BASE every session
    /// starts from, overridden per key by `[accounts.env]` and then by
    /// the project's env. Merged into `LoadedAccount.env` at
    /// load; the project layer is applied at spawn. Absent -> empty.
    #[serde(default)]
    env: HashMap<String, String>,
}

/// One `[gateway]` table. Unknown fields are rejected so a mistyped
/// key fails the load instead of being ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewaySettings {
    /// The inference listener's port. Fixed by default.
    #[serde(default)]
    port: Option<u16>,
    /// How many consecutive 429s from one account fire a rotation.
    #[serde(default)]
    streak_count: Option<u32>,
    /// The window the 429 streak is counted over, in seconds.
    #[serde(default)]
    streak_window_secs: Option<u64>,
    /// The cooldown when no reset time is known, in seconds.
    #[serde(default)]
    no_reset_cooldown_secs: Option<u64>,
}

impl GatewaySettings {
    /// The resolved port, refusing 0: the port is fixed by design, and
    /// a 0 here would ask the OS for an ephemeral port no child was
    /// ever told about.
    fn resolved_port(&self, path: &Path) -> Result<u16, WorkspaceError> {
        match self.port {
            Some(0) => Err(WorkspaceError::GatewayPortInvalid { path: path.to_path_buf() }),
            Some(port) => Ok(port),
            None => Ok(DEFAULT_GATEWAY_PORT),
        }
    }

    /// The resolved rotation numbers, refusing 0 on any of them: each
    /// 0 would silently disable the mechanism it configures, which
    /// reads as a bug rather than a choice. A magnitude above u32
    /// seconds is refused the same way - silently mapping it to the
    /// default is the substitution this refusal exists to prevent.
    fn resolved_rotation(
        &self,
        path: &Path,
    ) -> Result<forge_gateway::rotation::RotationNumbers, WorkspaceError> {
        fn refused_u32(
            path: &Path,
            key: &'static str,
            value: Option<u32>,
            default: u32,
        ) -> Result<u32, WorkspaceError> {
            match value {
                Some(0) => {
                    Err(WorkspaceError::GatewayRotationInvalid { path: path.to_path_buf(), key })
                }
                Some(v) => Ok(v),
                None => Ok(default),
            }
        }
        fn refused_duration(
            path: &Path,
            key: &'static str,
            value: Option<u64>,
            default: Duration,
        ) -> Result<Duration, WorkspaceError> {
            match value {
                Some(0) => {
                    Err(WorkspaceError::GatewayRotationInvalid { path: path.to_path_buf(), key })
                }
                Some(v) => u32::try_from(v).map(u64::from).map(Duration::from_secs).map_err(|_| {
                    WorkspaceError::GatewayRotationInvalid { path: path.to_path_buf(), key }
                }),
                None => Ok(default),
            }
        }
        let defaults = forge_gateway::rotation::RotationNumbers::default();
        Ok(forge_gateway::rotation::RotationNumbers {
            streak_count: refused_u32(
                path,
                "streak_count",
                self.streak_count,
                defaults.streak_count,
            )?,
            streak_window: refused_duration(
                path,
                "streak_window_secs",
                self.streak_window_secs,
                defaults.streak_window,
            )?,
            no_reset_cooldown: refused_duration(
                path,
                "no_reset_cooldown_secs",
                self.no_reset_cooldown_secs,
                defaults.no_reset_cooldown,
            )?,
        })
    }
}

impl ProjectEntry {
    /// The resolved mode for the project. Absent is `auto`, not "no
    /// override": under account rotation a session can start on any
    /// account in its org's pool, so the project is the stable scope
    /// and a forge default is what keeps its sessions consistent.
    fn permission_mode(
        &self,
        path: &std::path::Path,
        name: &str,
    ) -> Result<PermissionMode, WorkspaceError> {
        match self.permission_mode.as_deref() {
            None => Ok(PermissionMode::Auto),
            Some(raw) => PermissionMode::from_wire(raw).ok_or_else(|| {
                WorkspaceError::ProjectInvalidPermissionMode {
                    path: path.to_path_buf(),
                    name: name.to_owned(),
                    value: raw.to_owned(),
                }
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct OrgEntry {
    name: String,
    /// Account `display_name`s every project in this org is allowed
    /// to spawn under. Required; cross-validated against `[[accounts]]`.
    accounts: Vec<String>,
    /// Fallback `display_name`s used when every pinned account is
    /// unavailable. Absent -> empty. Validated against `[[accounts]]`
    /// exactly like `accounts`.
    #[serde(default)]
    fallback_accounts: Vec<String>,
    #[serde(default)]
    projects: Vec<ProjectEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectEntry {
    name: String,
    path: String,
    /// When `true`, the project's lead session spawns automatically
    /// at forge launch. Multiple projects can carry this; they all
    /// spawn in the background, and from the launchpad none of them
    /// is focused until the user picks one. Defaults to `false`.
    #[serde(default)]
    auto_start: bool,
    /// The project's model: fills the CLI's model slots at spawn
    /// (`ANTHROPIC_DEFAULT_*_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`,
    /// `CLAUDE_CODE_SUBAGENT_MODEL`). Required to be declared by at
    /// least one account in the org.
    #[serde(default)]
    model: Option<String>,
    /// CLI permission mode stamped onto every session this project
    /// spawns. Absent resolves to `auto`.
    #[serde(default)]
    permission_mode: Option<String>,
    /// Cap on this project's concurrently live dynamic workers.
    /// Absent keeps the default.
    #[serde(default)]
    max_workers: Option<usize>,
    /// Per-project environment entries, layered over the account's
    /// env at spawn.
    #[serde(default)]
    env: HashMap<String, String>,
    /// Path to a `KEY=value` file whose entries join this project's
    /// env, so a secret can live outside forge.toml. Read once at
    /// load, like every other value here, so rotating it needs a
    /// forge restart.
    #[serde(default)]
    env_file: Option<String>,
}

/// Unknown fields are rejected so a near-miss key (`providers`) fails
/// loudly instead of loading and leaving the account probing the wrong
/// endpoint until preflight hangs on it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountEntry {
    display_name: String,
    /// Which backend this account talks to. Required: an account that
    /// does not say probes the wrong endpoint and bails at preflight,
    /// so silence is the dangerous answer. Held as `Option` only so the
    /// absent case can name the account; `None` is a load error.
    provider: Option<forge_primitives::account::Provider>,
    /// The flat base URL. Required for base-url providers; an
    /// Anthropic account may omit it (the gateway constant is its
    /// upstream).
    #[serde(default)]
    base_url: Option<String>,
    /// The credential the gateway forwards, mapped onto the right
    /// variable per provider. Required.
    #[serde(default)]
    token: Option<String>,
    /// The canonical model names the account serves. Required
    /// non-empty.
    #[serde(default)]
    models: Vec<String>,
    /// Canonical name -> upstream slug, only where the spellings
    /// differ.
    #[serde(default)]
    model_slugs: HashMap<String, String>,
    /// Canonical name -> the other names a request may arrive under for
    /// that model.
    #[serde(default)]
    model_aliases: HashMap<String, Vec<String>>,
    /// Provider-behaviour extras only - timeouts, context caps,
    /// fallback switches. Base-url and credential keys are rejected:
    /// they are the flat keys' job.
    #[serde(default)]
    env: HashMap<String, String>,
}

// The loaded account shape lives in forge-primitives: the gateway
// builds its account state from it, so it crosses a crate boundary.
// Re-exported here so `crate::config::LoadedAccount` keeps resolving.
pub(crate) use forge_primitives::account::LoadedAccount;

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

/// Cap on a project's concurrently live dynamic workers when its
/// `max_workers` override is absent.
pub(crate) const DEFAULT_MAX_WORKERS_PER_PROJECT: usize = 2;

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
    /// `[[slack]]` workspaces. An absent or empty list leaves the
    /// connector dormant, the way an absent `[gotify]` does.
    pub slack: Vec<SlackConfig>,
    /// `[plugins]` section knobs. Absent section means auto-update is
    /// off.
    pub plugins: PluginSettings,
    /// The port the gateway's inference listener binds. Absent
    /// `[gateway]` section keeps the default.
    pub gateway_port: u16,
    /// The gateway's rotation numbers. Absent keys keep the defaults.
    pub gateway_rotation: forge_gateway::rotation::RotationNumbers,
}

/// The port the gateway's listener binds when `[gateway] port` is
/// absent. Fixed, not OS-assigned: every session's base URL names it,
/// so a drift would point children at an address nothing serves.
pub const DEFAULT_GATEWAY_PORT: u16 = 8787;

#[derive(Debug, Clone)]
pub(crate) struct LoadedProject {
    pub name: String,
    pub path: PathBuf,
    /// Original path string from `forge.toml`, preserved for display
    /// (e.g. `~/Projects/forge` with `~` un-expanded). Use `path` for
    /// filesystem access; this for human-readable output.
    pub display_path: String,
    /// Name of the org this project belongs to (matches
    /// `LoadedOrg.name`). The spawn resolves the org's pin and walk
    /// order through this back-reference.
    pub org: String,
    /// Cached pinned account list from the project's org. Duplicated
    /// here so callers don't need to walk the org list on every
    /// resolution.
    pub accounts: Vec<String>,
    /// Cached fallback list from the project's org, alongside
    /// `accounts`. Absent key -> empty.
    pub fallback_accounts: Vec<String>,
    /// `true` when the project should spawn automatically at forge
    /// launch.
    pub auto_start: bool,
    /// The project's model: fills the CLI's model slots at spawn
    /// (`ANTHROPIC_DEFAULT_*_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`,
    /// `CLAUDE_CODE_SUBAGENT_MODEL`) and is the model the walk matches
    /// accounts on. A project that declares none cannot spawn.
    pub model: Option<String>,
    /// Per-project environment from the entry's env table, layered
    /// over the account's env at spawn. An `ANTHROPIC_BASE_URL` or
    /// `ANTHROPIC_AUTH_TOKEN` here desyncs forge's own accounting -
    /// the usage probe and the selection walk both read the ACCOUNT
    /// map, so they measure a different endpoint.
    pub env: HashMap<String, String>,
    /// Cap on this project's live dynamic workers; `None` keeps the
    /// default. See `ProjectEntry::max_workers`.
    pub max_workers: Option<usize>,
    /// CLI permission mode stamped onto every session this project
    /// spawns. Resolved once at load; absent key resolves to `auto`.
    pub permission_mode: PermissionMode,
}

/// Complete `[env]` < `[accounts.env]` < the project's env,
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
            slack: Vec::new(),
            plugins: PluginSettings::default(),
            gateway_port: DEFAULT_GATEWAY_PORT,
            gateway_rotation: forge_gateway::rotation::RotationNumbers::default(),
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

    if parsed.workers.is_some() {
        tracing::warn!(
            target: "forge_workspace::config",
            event_name = "workers_section_ignored",
            "[workers] is no longer read; set max_workers on the project's \
             [[orgs.projects]] entry",
        );
    }

    if parsed.projects.is_some() {
        tracing::warn!(
            target: "forge_workspace::config",
            event_name = "projects_section_ignored",
            "[projects.<name>] is no longer read; move model, permission_mode, \
             max_workers, env and env_file onto the project's \
             [[orgs.projects]] entry - \
             this config's per-project keys are being silently dropped",
        );
    }

    if parsed.orgs.is_empty() {
        return Err(WorkspaceError::NoOrgsConfigured { path });
    }
    if parsed.accounts.is_empty() {
        return Err(WorkspaceError::NoAccountsConfigured { path });
    }

    // Global `[env]` is the BASE each account's effective env starts
    // from; the account's own `[accounts.env]` extends it, so account
    // keys override global keys.
    let mut global_env = parsed.env;

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
    for mut entry in parsed.accounts {
        if !seen_account_names.insert(entry.display_name.clone()) {
            return Err(WorkspaceError::DuplicateAccount { path, name: entry.display_name });
        }
        let Some(provider) = entry.provider else {
            return Err(WorkspaceError::AccountsMissingProvider {
                path,
                names: vec![entry.display_name],
            });
        };
        let mut env = global_env.clone();
        // A gateway key declared in an env layer is inert: the stamp
        // lands last over the merged env and overwrites all four. A
        // blank one still reads as absent rather than being carried
        // downstream as an empty credential.
        let gateway_keys = [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ];
        for key in gateway_keys {
            for env in [&mut global_env, &mut entry.env] {
                if env.get(key).is_some_and(|v| v.trim().is_empty()) {
                    env.remove(key);
                }
            }
        }
        env.extend(entry.env);
        trim_setup_token(&mut env);
        // The flat credential: mapped onto the provider's own variable
        // below, which is what the probe, the gateway forward and the
        // child stamp all read.
        let token = match entry.token.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => t.to_owned(),
            _ => {
                return Err(WorkspaceError::AccountTokenRequired {
                    path,
                    name: entry.display_name,
                });
            }
        };
        if entry.models.is_empty() {
            return Err(WorkspaceError::AccountModelsRequired { path, name: entry.display_name });
        }
        for (slug_key, slug_value) in &entry.model_slugs {
            if !entry.models.contains(slug_key) {
                return Err(WorkspaceError::AccountSlugUndeclared { path, slug: slug_key.clone() });
            }
            if slug_value.trim().is_empty() {
                return Err(WorkspaceError::AccountSlugBlank { path, slug: slug_key.clone() });
            }
        }
        for (alias_key, alias_values) in &entry.model_aliases {
            if !entry.models.contains(alias_key) {
                return Err(WorkspaceError::AccountAliasUndeclared {
                    path,
                    alias: alias_key.clone(),
                });
            }
            if alias_values.is_empty() {
                return Err(WorkspaceError::AccountAliasEmpty { path, alias: alias_key.clone() });
            }
        }
        let base_url =
            entry.base_url.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        // A base-url provider probes `{base_url}/...`, so an absent key
        // would leave the probe pointed at Anthropic's host with the
        // wrong bearer. Refuse at load rather than at preflight.
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
            && !base_url.as_deref().is_some_and(|v| v.trim_end_matches('/').ends_with("/api"))
        {
            return Err(WorkspaceError::OpenrouterBaseUrlNotApiRoot {
                path,
                name: entry.display_name,
            });
        }
        // Legal but self-inconsistent: the base is still stamped on the
        // spawned session, so chat goes to that endpoint while usage
        // probes the official API with the setup token.
        if provider == Provider::Anthropic && base_url.is_some() {
            tracing::warn!(
                target: "forge_workspace::config",
                account = %entry.display_name,
                "anthropic account declares a base_url; sessions will use that endpoint \
                 while usage probes the official API",
            );
        }
        // Map the flat keys onto the provider's variables - what the
        // probe, the gateway forward and the child stamp all read.
        if let Some(base_url) = &base_url {
            env.insert("ANTHROPIC_BASE_URL".to_owned(), base_url.clone());
        }
        let credential_variable = forge_gateway::binding::credential_variable_for(provider);
        env.insert(credential_variable.to_owned(), token);
        accounts.push(LoadedAccount {
            display_name: entry.display_name,
            provider,
            base_url,
            models: entry.models,
            model_slugs: entry.model_slugs,
            model_aliases: entry.model_aliases,
            env,
        });
    }

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
        for account in org_entry.accounts.iter().chain(&org_entry.fallback_accounts) {
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
            let permission_mode = project_entry.permission_mode(&path, &project_entry.name)?;
            let env =
                resolve_project_env(&project_entry.name, project_entry.env, project_entry.env_file);
            projects.push(LoadedProject {
                name: project_entry.name.clone(),
                path: expand_home(&project_entry.path),
                display_path: project_entry.path,
                org: org_entry.name.clone(),
                accounts: org_entry.accounts.clone(),
                fallback_accounts: org_entry.fallback_accounts.clone(),
                auto_start: project_entry.auto_start,
                model: project_entry.model.clone(),
                env,
                max_workers: project_entry.max_workers,
                permission_mode,
            });
            // The project model must be served by at least one account
            // the org can reach: a typo would boot clean, stamp all the
            // CLI slots, and then 503 every session's first request.
            if let Some(model) = &project_entry.model {
                let served =
                    org_entry.accounts.iter().chain(&org_entry.fallback_accounts).any(|name| {
                        accounts.iter().any(|a| {
                            a.display_name == *name
                                && forge_gateway::account::account_serves(
                                    &a.models,
                                    &a.model_aliases,
                                    model,
                                )
                        })
                    });
                if !served {
                    return Err(WorkspaceError::ProjectModelUndeclared {
                        path,
                        name: project_entry.name.clone(),
                        model: model.clone(),
                    });
                }
            }
        }
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

    let gateway_port = match &parsed.gateway {
        Some(gateway) => gateway.resolved_port(&path)?,
        None => DEFAULT_GATEWAY_PORT,
    };
    let gateway_rotation = match &parsed.gateway {
        Some(gateway) => gateway.resolved_rotation(&path)?,
        None => forge_gateway::rotation::RotationNumbers::default(),
    };

    Ok(LoadedConfig {
        projects,
        default_index,
        accounts,
        ui: parsed.ui,
        dictate: parsed.dictate,
        gotify: parsed.gotify,
        slack: parsed.slack,
        plugins: parsed.plugins,
        gateway_port,
        gateway_rotation,
    })
}

/// A project's env: the `env_file` entries with the inline `env` table
/// layered over them, since the inline form is the more explicit
/// statement of the two.
/// Trim the setup token once here: the probe and the spawned child
/// both read these maps verbatim, so a padded value would authenticate
/// one and fail the other.
fn trim_setup_token<S: std::hash::BuildHasher>(env: &mut HashMap<String, String, S>) {
    if let Some(token) = env.get_mut(forge_gateway::CLAUDE_CODE_OAUTH_TOKEN_ENV) {
        *token = token.trim().to_owned();
    }
}

fn resolve_project_env(
    project: &str,
    env: HashMap<String, String>,
    env_file: Option<String>,
) -> HashMap<String, String> {
    let mut merged = env_file.map(|path| read_env_file(project, &path)).unwrap_or_default();
    merged.extend(env);
    trim_setup_token(&mut merged);
    merged
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
            "the project's env_file entry did not fully apply",
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
                // moved out of the project's env table arrives with
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
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#
    }

    /// `[gateway] port = 0` is refused at load: the port is fixed, and
    /// a 0 would ask the OS for an ephemeral port no child was told
    /// about.
    #[test]
    fn gateway_port_zero_is_refused_at_load() {
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
auto_start = true

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[gateway]
port = 0
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("port 0 must not load");
        assert!(
            err.to_string().contains("port 0"),
            "the error names the unusable port, got: {err}",
        );
    }

    #[test]
    fn gateway_port_defaults_when_the_section_is_absent() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("absent section loads");
        assert_eq!(config.gateway_port, DEFAULT_GATEWAY_PORT);
    }

    #[test]
    fn gateway_rotation_numbers_default_when_keys_are_absent() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("absent keys load");
        assert_eq!(config.gateway_rotation, forge_gateway::rotation::RotationNumbers::default(),);
    }

    #[test]
    fn gateway_rotation_numbers_parse_from_the_section() {
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
auto_start = true

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[gateway]
streak_count = 3
streak_window_secs = 45
no_reset_cooldown_secs = 90
"#,
        );
        let config = load_from_dir(dir.path()).expect("section loads");
        assert_eq!(config.gateway_rotation.streak_count, 3);
        assert_eq!(config.gateway_rotation.streak_window, Duration::from_secs(45));
        assert_eq!(config.gateway_rotation.no_reset_cooldown, Duration::from_secs(90));
    }

    #[test]
    fn gateway_rotation_zero_keys_are_refused_at_load() {
        // An absurd magnitude is refused with the same error: silently
        // mapping it to the default would substitute the user's value.
        let values = [
            ("streak_count", "0"),
            ("streak_window_secs", "0"),
            ("streak_window_secs", "99999999999"),
            ("no_reset_cooldown_secs", "0"),
            ("no_reset_cooldown_secs", "99999999999"),
        ];
        for (key, value) in values {
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
auto_start = true

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[gateway]
{key} = {value}
"#
                ),
            );
            let err = load_from_dir(dir.path()).expect_err("an unusable key must not load");
            assert!(err.to_string().contains(key), "the error names the unusable key, got: {err}");
        }
    }

    #[test]
    fn parses_the_flat_base_url_and_token_onto_the_provider_variables() {
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
token = "unused"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        assert_eq!(account.display_name, "Codex");
        assert_eq!(
            account.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://localhost:18765"),
            "the flat base_url stamps the provider's base-url variable",
        );
        assert_eq!(
            account.env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("unused"),
            "the flat token stamps the provider's credential variable",
        );
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("codex without a base url must not load");
        let message = err.to_string();
        assert!(
            message.contains("Codex") && message.contains("base_url"),
            "the error has to name the account and the missing key, got: {message}",
        );
    }

    /// `https://openrouter.ai/v1/key` answers 200 with a marketing page,
    /// and 200 is the one status the probe treats as success, so a bare
    /// host reaches the decode arm, retries twelve times and bails the
    /// account - which stops forge starting. Catch the base at load
    /// instead, where the user can act on it.
    #[test]
    fn a_blank_slug_value_fails_the_load() {
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
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
[accounts.model_slugs]
"claude-sonnet-5" = "   "
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("a blank slug value must not load");
        let message = err.to_string();
        assert!(message.contains("blank slug"), "the error names the blank slug, got: {message}");
    }

    #[test]
    fn a_project_model_no_account_declares_fails_the_load() {
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
model = "gpt-5.6-luna"
[[accounts]]
display_name = "Codex"
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("an undeclared project model must not load");
        let message = err.to_string();
        assert!(
            message.contains("gpt-5.6-luna") && message.contains("forge"),
            "the error names the project and the model, got: {message}",
        );
    }

    #[test]
    fn a_project_model_an_org_account_declares_loads() {
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
model = "claude-sonnet-5"
[[accounts]]
display_name = "Codex"
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
"#,
        );
        let config = load_from_dir(dir.path()).expect("a declared model loads");
        assert_eq!(config.projects[0].model.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn a_project_model_that_is_an_alias_loads() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Granite"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
model = "claude-opus-5"
[[accounts]]
display_name = "Granite"
token = "t"
models = ["claude-opus-5[1m]"]
provider = "anthropic"
[accounts.model_aliases]
"claude-opus-5[1m]" = ["claude-opus-5"]
"#,
        );
        let config = load_from_dir(dir.path()).expect("an aliased project model loads");
        assert_eq!(config.projects[0].model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn a_project_model_that_is_neither_declared_nor_an_alias_fails_the_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Granite"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
model = "claude-sonnet-5"
[[accounts]]
display_name = "Granite"
token = "t"
models = ["claude-opus-5[1m]"]
provider = "anthropic"
[accounts.model_aliases]
"claude-opus-5[1m]" = ["claude-opus-5"]
"#,
        );
        let err = load_from_dir(dir.path())
            .expect_err("holding aliases must not make the account serve everything");
        let message = err.to_string();
        assert!(
            message.contains("claude-sonnet-5"),
            "the error names the undeclared model, got: {message}",
        );
    }

    #[test]
    fn an_alias_for_an_undeclared_model_fails_the_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Granite"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Granite"
token = "t"
models = ["claude-opus-5[1m]"]
provider = "anthropic"
[accounts.model_aliases]
"claude-opus-5" = ["claude-opus-5[1m]"]
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("an undeclared alias key must not load");
        let message = err.to_string();
        assert!(
            message.contains("'claude-opus-5'"),
            "the error quotes the undeclared alias key, not the declared marker, got: {message}",
        );
    }

    #[test]
    fn an_empty_alias_list_fails_the_load() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Granite"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Granite"
token = "t"
models = ["claude-opus-5[1m]"]
provider = "anthropic"
[accounts.model_aliases]
"claude-opus-5[1m]" = []
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("an empty alias list must not load");
        let message = err.to_string();
        assert!(
            message.contains("empty alias list"),
            "the error says the list is empty, got: {message}",
        );
    }

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
token = "t"
models = ["claude-sonnet-5"]
provider = "openrouter"
base_url = "https://openrouter.ai"
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
token = "t"
models = ["claude-sonnet-5"]
provider = "openrouter"
base_url = "https://openrouter.ai/api/"
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
token = "t"
models = ["claude-sonnet-5"]
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
    fn project_entry_rejects_an_unknown_key() {
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
auto_start = true
gatewy = true
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        // The near-miss matters: a project key that loaded as nothing
        // would silently drop whatever behaviour it configured.
        let err = load_from_dir(dir.path()).expect_err("a mistyped project key must not load");
        let message = err.to_string();
        assert!(
            message.contains("gatewy"),
            "the error has to name the offending key, got: {message}",
        );
    }

    #[test]
    fn slack_section_parses_and_rejects_an_unknown_key() {
        let toml = r#"
[[slack]]
workspace = "acme"
token = "xoxp-test"
poll_seconds = 45
"#;
        let parsed: ForgeToml = toml::from_str(toml).expect("slack section parses");
        assert_eq!(parsed.slack.len(), 1);
        assert_eq!(parsed.slack[0].workspace, "acme");
        assert_eq!(parsed.slack[0].poll_seconds, 45);

        let bad = r#"
[[slack]]
workspace = "acme"
token = "xoxp-test"
poll_second = 45
"#;
        let err = toml::from_str::<ForgeToml>(bad).expect_err("a near-miss key must fail loudly");
        assert!(err.to_string().contains("poll_second"), "got: {err}");
    }

    #[test]
    fn slack_poll_seconds_defaults_to_thirty() {
        let toml = "[[slack]]\nworkspace = \"a\"\ntoken = \"x\"\n";
        let parsed: ForgeToml = toml::from_str(toml).expect("parses without poll_seconds");
        assert_eq!(parsed.slack[0].poll_seconds, 30);
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
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "   "
"#,
        );
        let err =
            load_from_dir(dir.path()).expect_err("a blank base url must not satisfy the check");
        assert!(err.to_string().contains("base_url"), "got: {err}");
    }

    #[test]
    fn account_without_env_table_stamps_the_derived_credential() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        // The flat token maps onto the provider's credential variable -
        // the only key an account without [accounts.env] carries.
        assert_eq!(
            account.env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some("t"),
            "the flat token lands on the provider's credential variable",
        );
        assert_eq!(account.env.len(), 1, "nothing else is injected");
    }

    /// A gateway key declared in an env layer loads and rides through
    /// untouched: the stamp is what makes it inert, and it lands after
    /// the layers merge, so a second load-time gate on the same keys
    /// would only decide the same question twice.
    #[test]
    fn a_gateway_key_in_an_env_layer_loads_and_rides_through() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[env]
ANTHROPIC_BASE_URL = "https://proxy.example"

[[orgs]]
name = "Personal"
accounts = ["Personal"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
[[accounts]]
display_name = "Personal"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
[accounts.env]
ANTHROPIC_API_KEY = "sk-ant-123"
ANTHROPIC_AUTH_TOKEN = "t2"
"#,
        );
        let config = load_from_dir(dir.path()).expect("a gateway env key is not refused");
        let env = &config.accounts[0].env;
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://proxy.example"),
            "the global layer's key survives the merge",
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-ant-123"),
            "the account layer's key survives the merge",
        );
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("t2"),
            "a key beside the flat token survives the merge",
        );
    }

    #[test]
    fn a_whitespace_gateway_key_reads_as_absent() {
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
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
[accounts.env]
ANTHROPIC_API_KEY = "   "
"#,
        );
        let config = load_from_dir(dir.path()).expect("a blank gateway key is absent");
        assert!(
            config.accounts[0].env.get("ANTHROPIC_API_KEY").is_none(),
            "a blank key is scrubbed, not carried downstream as an empty credential",
        );
    }

    #[test]
    fn an_account_without_a_token_fails_the_load() {
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
models = ["claude-sonnet-5"]
provider = "codex"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("a tokenless account must not load");
        let message = err.to_string();
        assert!(
            message.contains("Codex") && message.contains("token"),
            "the error names the account and the missing key, got: {message}",
        );
    }

    #[test]
    fn an_account_without_models_fails_the_load() {
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
token = "t"
provider = "codex"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("a modelless account must not load");
        let message = err.to_string();
        assert!(
            message.contains("models"),
            "the error names the missing declaration, got: {message}",
        );
    }

    #[test]
    fn a_slug_for_an_undeclared_model_fails_the_load() {
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
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
[accounts.model_slugs]
"deepseek/deepseek-v4.1-flash" = "deepseek/deepseek-v4.1-flash"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("an undeclared slug key must not load");
        let message = err.to_string();
        assert!(
            message.contains("deepseek/deepseek-v4.1-flash"),
            "the error names the undeclared slug key, got: {message}",
        );
    }

    #[test]
    fn project_permission_mode_defaults_to_auto_when_absent() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("absent key must not block the load");
        assert_eq!(
            config.projects[0].permission_mode,
            PermissionMode::Auto,
            "a project with no per-project keys at all still lands on auto",
        );
    }

    #[test]
    fn project_permission_mode_defaults_to_auto_when_the_table_omits_it() {
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
max_workers = 2
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("a table without the key loads");
        assert_eq!(
            config.projects[0].permission_mode,
            PermissionMode::Auto,
            "a project whose table omits the key gets auto, not the launcher default",
        );
    }

    #[test]
    fn project_permission_mode_parses_an_explicit_value() {
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
permission_mode = "bypassPermissions"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(
            config.projects[0].permission_mode,
            PermissionMode::BypassPermissions,
            "a valid mode lands on the LoadedProject verbatim",
        );
    }

    #[test]
    fn project_permission_mode_accepts_the_snake_case_alias() {
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
permission_mode = "bypass_permissions"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("the from_wire aliases still work");
        assert_eq!(
            config.projects[0].permission_mode,
            PermissionMode::BypassPermissions,
            "the alias set must not shrink now the key moved tables",
        );
    }

    #[test]
    fn project_with_invalid_permission_mode_fails_naming_the_accepted_set() {
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
permission_mode = "yolo"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("an invalid mode must not load");
        let message = err.to_string();
        assert!(
            message.contains("yolo") && message.contains("forge"),
            "the error has to name the offending value and project, got: {message}",
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
permissionmode = "bypassPermissions"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("a mistyped project key must not load");
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
[accounts.env]
CLAUDE_CODE_AUTO_COMPACT_WINDOW = "372000"
[[accounts]]
display_name = "Gateway"
token = "t"
models = ["claude-sonnet-5"]
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

[orgs.projects.env]
CLAUDE_CODE_OAUTH_TOKEN = "  sk-ant-oat01-project  "

[[accounts]]
display_name = "Stargate"
token = "  sk-ant-oat01-stargate  "
models = ["claude-sonnet-5"]
provider = "anthropic"
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
            forge_gateway::token_bearer(&account.env),
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
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let account = &config.accounts[0];
        assert_eq!(account.env.len(), 2, "no [env] -> env is exactly the derived stamps");
        assert_eq!(
            account.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://localhost:18765"),
        );
        assert_eq!(account.env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str), Some("t"),);
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
env = { ALL_THREE = "project", GLOBAL_PROJECT = "project", PROJECT_ONLY = "project" }

[[accounts]]
display_name = "Codex"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
[accounts.env]
ALL_THREE = "account"
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
        let cases = [("mistyped inner table", "envs = { K = \"v\" }\n", "envs")];
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
        // The inline parameter is an `env` entry fragment when set: the
        // collocated shape carries the env inline table on the entry.
        let inline_line =
            if inline.is_empty() { String::new() } else { format!("\nenv = {{ {inline} }}") };
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
env_file = "{file}"{inline_line}
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
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
            "SHARED = \"from-inline\"",
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
env_file = "{}/nope.env"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
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
env = { AIRMAIL_MCP_URL = "https://mail.example/mcp" }
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let project = &config.projects[0];
        assert_eq!(
            project.env.get("AIRMAIL_MCP_URL").map(String::as_str),
            Some("https://mail.example/mcp"),
            "the project env block lands on the named project",
        );
    }

    /// An env block naming a project that no `[[orgs.projects]]`
    /// declares is the same typo class as an org naming an undeclared
    /// account, which `load_from_dir` already refuses to boot on. A
    /// silently-ignored env block is the failure mode #551 exists to
    /// kill, so it must not load.
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
env = { AIRMAIL_TOKEN = "forge-only-secret" }
[[orgs.projects]]
name = "airmail"
path = "~/Projects/airmail"

[[accounts]]
display_name = "Codex"
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
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

    /// The collocation's load-bearing mechanic: each block-form env
    /// attaches to the most recent `[[orgs.projects]]` header, so two
    /// env blocks under two entries land on their OWN projects. The
    /// single-project form cannot pin this - with one project there is
    /// nothing to mis-attach to.
    #[test]
    fn two_block_form_envs_attach_to_their_own_entries() {
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

[orgs.projects.env]
FORGE_KEY = "forge-secret"
[[orgs.projects]]
name = "airmail"
path = "~/Projects/airmail"

[orgs.projects.env]
AIRMAIL_KEY = "airmail-secret"
[[accounts]]
display_name = "Codex"
token = "t"
models = ["claude-sonnet-5"]
provider = "codex"
base_url = "http://localhost:18765"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let forge = config.projects.iter().find(|p| p.name == "forge").expect("forge");
        let airmail = config.projects.iter().find(|p| p.name == "airmail").expect("airmail");
        assert_eq!(
            forge.env.get("FORGE_KEY").map(String::as_str),
            Some("forge-secret"),
            "the first entry's env block attaches to the first project",
        );
        assert!(
            !forge.env.contains_key("AIRMAIL_KEY"),
            "the second block must not leak onto the first project: {:?}",
            forge.env,
        );
        assert_eq!(
            airmail.env.get("AIRMAIL_KEY").map(String::as_str),
            Some("airmail-secret"),
            "the second entry's env block attaches to the second project",
        );
        assert!(
            !airmail.env.contains_key("FORGE_KEY"),
            "the first block must not leak onto the second project: {:?}",
            airmail.env,
        );
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
    fn the_projects_max_workers_key_reaches_the_loaded_project() {
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
auto_start = true
max_workers = 4

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.default_project().max_workers, Some(4));
    }

    #[test]
    fn absent_max_workers_leaves_the_project_at_the_default() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.default_project().max_workers, None);
    }

    #[test]
    fn a_negative_max_workers_fails_the_load() {
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
max_workers = -1

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let error = load_from_dir(dir.path()).expect_err("a bad value must fail loudly");
        assert!(error.to_string().contains("max_workers"), "names the key: {error}");
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
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
    fn org_fallback_accounts_parse_and_inherit_to_projects() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Tiered"
accounts = ["Codex"]
fallback_accounts = ["Router"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[orgs]]
name = "Plain"
accounts = ["Codex"]

[[orgs.projects]]
name = "spare"
path = "~/Projects/spare"

[[accounts]]
display_name = "Codex"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[[accounts]]
display_name = "Router"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let config = load_from_dir(dir.path()).expect("fallback_accounts parse");
        let forge = named(&config, "forge");
        assert_eq!(forge.fallback_accounts, vec!["Router"]);
        let spare = named(&config, "spare");
        assert!(
            spare.fallback_accounts.is_empty(),
            "absent fallback_accounts -> empty vec, not an error",
        );
    }

    #[test]
    fn unknown_fallback_account_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Stargate"]
fallback_accounts = ["Bogus"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("unknown fallback should error");
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
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
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
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
