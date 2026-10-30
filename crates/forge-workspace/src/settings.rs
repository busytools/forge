//! The class catalogue and the effective-config document.
//!
//! One table lists every key the loader accepts, the area it draws in and
//! when an edit to it takes effect; each entry also resolves its own value
//! from a `LoadedConfig`. The reload summary and the settings page both
//! read that table, so a key cannot be live in one and next-session in the
//! other.

use std::collections::BTreeSet;
use std::path::Path;

use forge_primitives::settings::SettingsTotals;
use forge_primitives::{EffectClass, SettingKind, SettingsArea, SettingsDocument, SettingsKey};
use serde_json::json;

use crate::config::{
    ConfigReload, DEFAULT_HANDOFF_AFTER_COMPACTIONS, DEFAULT_HANDOFF_AT_PERCENT,
    DEFAULT_MAX_WORKERS_PER_PROJECT, LoadedConfig, LoadedProject,
};

/// One area's fixed identity: the id the page keys on, the name it draws,
/// and the line under it.
pub struct AreaSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub note: &'static str,
}

/// The sub-nav's areas, in the order it draws them. `[browser]` rides with
/// the server that hosts it and `[dictate]` with the client that captures
/// through it, so the eleven here cover every table without a second rail
/// entry for either.
pub const AREAS: &[AreaSpec] = &[
    AreaSpec {
        id: "accounts",
        name: "accounts",
        note: "the provider endpoints and credentials a session can spawn under",
    },
    AreaSpec {
        id: "projects",
        name: "projects",
        note: "orgs, their projects, and every per-project setting",
    },
    AreaSpec {
        id: "connectors",
        name: "connectors",
        note: "gotify, slack, system one and the plugin switch - the integrations that reach outside forge",
    },
    AreaSpec {
        id: "server",
        name: "server",
        note: "the socket a client reads through, and the browser the server hosts",
    },
    AreaSpec {
        id: "client",
        name: "client",
        note: "the sets a client draws with, and the dictation it captures through",
    },
    AreaSpec {
        id: "models",
        name: "models",
        note: "per-model parameters stamped onto a spawned child",
    },
    AreaSpec {
        id: "handoff",
        name: "handoff",
        note: "when a session swaps itself for a fresh one",
    },
    AreaSpec {
        id: "documents",
        name: "documents",
        note: "the ceiling on one document a session hands you",
    },
    AreaSpec { id: "usage", name: "usage", note: "how often the account pool refreshes its usage" },
    AreaSpec {
        id: "gateway",
        name: "gateway",
        note: "the inference listener sessions are pointed at",
    },
    AreaSpec { id: "env", name: "env", note: "variables stamped onto every spawned child" },
];

/// One row a catalogued key resolves to: a concrete path (with the instance
/// where the table repeats) and the effective value.
#[derive(Clone, PartialEq)]
struct KeyRow {
    path: String,
    kind: SettingKind,
    value: serde_json::Value,
}

fn text(path: impl Into<String>, value: impl Into<String>) -> KeyRow {
    KeyRow { path: path.into(), kind: SettingKind::String, value: json!(value.into()) }
}

/// An optional setting's raw value: the value when the file set it, and
/// `null` when it did not. The effective built-in is the page's to draw, so
/// the client's axes path never reads a display default back as a setting.
fn text_set(path: impl Into<String>, value: Option<String>) -> KeyRow {
    KeyRow {
        path: path.into(),
        kind: SettingKind::String,
        value: value.map_or(serde_json::Value::Null, serde_json::Value::String),
    }
}

fn number(path: impl Into<String>, value: impl serde::Serialize) -> KeyRow {
    KeyRow { path: path.into(), kind: SettingKind::Number, value: json!(value) }
}

fn flag(path: impl Into<String>, value: bool) -> KeyRow {
    KeyRow { path: path.into(), kind: SettingKind::Bool, value: json!(value) }
}

fn list(path: impl Into<String>, values: Vec<String>) -> KeyRow {
    let value =
        serde_json::Value::Array(values.into_iter().map(serde_json::Value::String).collect());
    KeyRow { path: path.into(), kind: SettingKind::List, value }
}

/// One accepted key: its pattern, the area it draws in, its class, and the
/// rows it resolves to for a load.
pub struct ConfigKey {
    pub path: &'static str,
    pub area: &'static str,
    pub class: EffectClass,
    rows: fn(&LoadedConfig) -> Vec<KeyRow>,
}

/// A key's class by its catalogue pattern, for the book's reconciliation
/// and the completeness test.
pub fn effect_class(path: &str) -> Option<EffectClass> {
    all_keys().find(|key| key.path == path).map(|key| key.class)
}

fn all_keys() -> impl Iterator<Item = &'static ConfigKey> {
    ACCOUNT_KEYS
        .iter()
        .chain(PROJECT_KEYS)
        .chain(CONNECTOR_KEYS)
        .chain(SERVER_KEYS)
        .chain(CLIENT_KEYS)
        .chain(MODEL_KEYS)
        .chain(HANDOFF_KEYS)
        .chain(DOCUMENT_KEYS)
        .chain(USAGE_KEYS)
        .chain(GATEWAY_KEYS)
        .chain(ENV_KEYS)
}

fn provider_label(provider: forge_primitives::account::Provider) -> &'static str {
    match provider {
        forge_primitives::account::Provider::Anthropic => "anthropic",
        forge_primitives::account::Provider::Codex => "codex",
        forge_primitives::account::Provider::Openrouter => "openrouter",
        forge_primitives::account::Provider::Zai => "zai",
    }
}

fn org_names(config: &LoadedConfig) -> Vec<&str> {
    let mut seen = BTreeSet::new();
    config.projects.iter().filter(|p| seen.insert(p.org.as_str())).map(|p| p.org.as_str()).collect()
}

fn org_project<'a>(config: &'a LoadedConfig, org: &str) -> Option<&'a LoadedProject> {
    config.projects.iter().find(|p| p.org == org)
}

fn sorted_keys<V>(map: &std::collections::HashMap<String, V>) -> Vec<&String> {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_unstable();
    keys
}

const ACCOUNT_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "accounts[].display_name",
        area: "accounts",
        class: EffectClass::Live,
        rows: |config| {
            config
                .accounts
                .iter()
                .map(|a| text(format!("accounts.{}.display_name", a.display_name), &a.display_name))
                .collect()
        },
    },
    ConfigKey {
        path: "accounts[].provider",
        area: "accounts",
        class: EffectClass::Live,
        rows: |config| {
            config
                .accounts
                .iter()
                .map(|a| {
                    text(
                        format!("accounts.{}.provider", a.display_name),
                        provider_label(a.provider),
                    )
                })
                .collect()
        },
    },
    ConfigKey {
        path: "accounts[].base_url",
        area: "accounts",
        class: EffectClass::Live,
        rows: |config| {
            config
                .accounts
                .iter()
                .map(|a| {
                    text(
                        format!("accounts.{}.base_url", a.display_name),
                        a.base_url.clone().unwrap_or_else(|| "not set".to_owned()),
                    )
                })
                .collect()
        },
    },
    ConfigKey {
        path: "accounts[].token",
        area: "accounts",
        class: EffectClass::Live,
        rows: |config| {
            config
                .accounts
                .iter()
                .map(|a| {
                    let variable = forge_gateway::binding::credential_variable_for(a.provider);
                    text(
                        format!("accounts.{}.token", a.display_name),
                        a.env.get(variable).cloned().unwrap_or_else(|| "not set".to_owned()),
                    )
                })
                .collect()
        },
    },
    ConfigKey {
        path: "accounts[].models",
        area: "accounts",
        class: EffectClass::Live,
        rows: |config| {
            config
                .accounts
                .iter()
                .map(|a| list(format!("accounts.{}.models", a.display_name), a.models.clone()))
                .collect()
        },
    },
    ConfigKey {
        path: "accounts[].model_slugs.*",
        area: "accounts",
        class: EffectClass::Live,
        rows: |config| {
            let mut rows = Vec::new();
            for account in &config.accounts {
                for name in sorted_keys(&account.model_slugs) {
                    rows.push(text(
                        format!("accounts.{}.model_slugs.{name}", account.display_name),
                        &account.model_slugs[name],
                    ));
                }
            }
            rows
        },
    },
    ConfigKey {
        path: "accounts[].model_aliases.*",
        area: "accounts",
        class: EffectClass::Live,
        rows: |config| {
            let mut rows = Vec::new();
            for account in &config.accounts {
                for name in sorted_keys(&account.model_aliases) {
                    rows.push(list(
                        format!("accounts.{}.model_aliases.{name}", account.display_name),
                        account.model_aliases[name].clone(),
                    ));
                }
            }
            rows
        },
    },
    ConfigKey {
        path: "accounts[].env.*",
        area: "accounts",
        class: EffectClass::NextSession,
        rows: |config| {
            let mut rows = Vec::new();
            for account in &config.accounts {
                let credential = forge_gateway::binding::credential_variable_for(account.provider);
                for name in sorted_keys(&account.env) {
                    let value = &account.env[name];
                    if name == "ANTHROPIC_BASE_URL"
                        || name == credential
                        || config.global_env.get(name) == Some(value)
                    {
                        continue;
                    }
                    rows.push(text(format!("accounts.{}.env.{name}", account.display_name), value));
                }
            }
            rows
        },
    },
];
const PROJECT_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "orgs[].name",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            org_names(config).into_iter().map(|org| text(format!("orgs.{org}.name"), org)).collect()
        },
    },
    ConfigKey {
        path: "orgs[].accounts",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            org_names(config)
                .into_iter()
                .filter_map(|org| {
                    org_project(config, org)
                        .map(|p| list(format!("orgs.{org}.accounts"), p.accounts.clone()))
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].fallback_accounts",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            org_names(config)
                .into_iter()
                .filter_map(|org| {
                    org_project(config, org).map(|p| {
                        list(format!("orgs.{org}.fallback_accounts"), p.fallback_accounts.clone())
                    })
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].browser_profile",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            org_names(config)
                .into_iter()
                .filter_map(|org| {
                    config
                        .org_browser_profiles
                        .get(org)
                        .map(|name| text(format!("orgs.{org}.browser_profile"), name))
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].slack.workspace",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            org_names(config)
                .into_iter()
                .filter_map(|org| {
                    config
                        .org_slack
                        .get(org)
                        .map(|workspace| text(format!("orgs.{org}.slack.workspace"), workspace))
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].name",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            config.projects.iter().map(|p| text(project_path(p, "name"), &p.name)).collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].path",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            config.projects.iter().map(|p| text(project_path(p, "path"), &p.display_path)).collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].auto_start",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .projects
                .iter()
                .map(|p| flag(project_path(p, "auto_start"), p.auto_start))
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].model",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .projects
                .iter()
                .map(|p| {
                    text(
                        project_path(p, "model"),
                        p.model.clone().unwrap_or_else(|| "not set".to_owned()),
                    )
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].permission_mode",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .projects
                .iter()
                .map(|p| text(project_path(p, "permission_mode"), p.permission_mode.as_wire()))
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].max_workers",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .projects
                .iter()
                .map(|p| {
                    number(
                        project_path(p, "max_workers"),
                        p.max_workers.unwrap_or(DEFAULT_MAX_WORKERS_PER_PROJECT),
                    )
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].env.*",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            let mut rows = Vec::new();
            for project in &config.projects {
                for name in sorted_keys(&project.env) {
                    rows.push(text(
                        project_path(project, &format!("env.{name}")),
                        &project.env[name],
                    ));
                }
            }
            rows
        },
    },
    ConfigKey {
        path: "orgs[].projects[].env_file",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .projects
                .iter()
                .filter_map(|p| {
                    p.env_file.as_ref().map(|path| text(project_path(p, "env_file"), path))
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].vault_roots",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .projects
                .iter()
                .map(|p| {
                    list(
                        project_path(p, "vault_roots"),
                        p.vault_roots.iter().map(|root| root.display().to_string()).collect(),
                    )
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].handoff.enabled",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            config
                .projects
                .iter()
                .filter_map(|project| {
                    let own = project.handoff?;
                    let policy = crate::config::HandoffPolicy::resolve(&config.handoff, Some(&own));
                    Some(flag(project_path(project, "handoff.enabled"), policy.enabled))
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].handoff.at_percent",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            config
                .projects
                .iter()
                .filter_map(|project| {
                    let own = project.handoff?;
                    let policy = crate::config::HandoffPolicy::resolve(&config.handoff, Some(&own));
                    Some(number(project_path(project, "handoff.at_percent"), policy.at_percent))
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].handoff.after_compactions",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            config
                .projects
                .iter()
                .filter_map(|project| {
                    let own = project.handoff?;
                    let policy = crate::config::HandoffPolicy::resolve(&config.handoff, Some(&own));
                    Some(number(
                        project_path(project, "handoff.after_compactions"),
                        policy.after_compactions,
                    ))
                })
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].browser_profile",
        area: "projects",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .projects
                .iter()
                .map(|p| text(project_path(p, "browser_profile"), p.browser_profile.name()))
                .collect()
        },
    },
    ConfigKey {
        path: "orgs[].projects[].slack.workspace",
        area: "projects",
        class: EffectClass::Live,
        rows: |config| {
            config
                .projects
                .iter()
                .filter_map(|p| {
                    p.slack_workspace
                        .as_ref()
                        .map(|workspace| text(project_path(p, "slack.workspace"), workspace))
                })
                .collect()
        },
    },
];

fn project_path(project: &LoadedProject, key: &str) -> String {
    format!("orgs.{}.projects.{}.{key}", project.org, project.name)
}
const CONNECTOR_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "gotify.url",
        area: "connectors",
        class: EffectClass::Live,
        rows: |config| {
            config.gotify.as_ref().map(|g| text("gotify.url", &g.url)).into_iter().collect()
        },
    },
    ConfigKey {
        path: "gotify.client_token",
        area: "connectors",
        class: EffectClass::Live,
        rows: |config| {
            config
                .gotify
                .as_ref()
                .map(|g| text("gotify.client_token", &g.client_token))
                .into_iter()
                .collect()
        },
    },
    ConfigKey {
        path: "slack[].workspace",
        area: "connectors",
        class: EffectClass::Restart,
        rows: |config| {
            config
                .slack
                .iter()
                .map(|s| text(format!("slack.{}.workspace", s.workspace), &s.workspace))
                .collect()
        },
    },
    ConfigKey {
        path: "slack[].token",
        area: "connectors",
        class: EffectClass::Restart,
        rows: |config| {
            config
                .slack
                .iter()
                .map(|s| text(format!("slack.{}.token", s.workspace), &s.token))
                .collect()
        },
    },
    ConfigKey {
        path: "slack[].poll_seconds",
        area: "connectors",
        class: EffectClass::Restart,
        rows: |config| {
            config
                .slack
                .iter()
                .map(|s| number(format!("slack.{}.poll_seconds", s.workspace), s.poll_seconds))
                .collect()
        },
    },
    ConfigKey {
        path: "slack[].dm_poll_seconds",
        area: "connectors",
        class: EffectClass::Restart,
        rows: |config| {
            config
                .slack
                .iter()
                .map(|s| {
                    number(format!("slack.{}.dm_poll_seconds", s.workspace), s.dm_poll_seconds)
                })
                .collect()
        },
    },
    ConfigKey {
        path: "slack[].thread_idle_days",
        area: "connectors",
        class: EffectClass::Live,
        rows: |config| {
            config
                .slack
                .iter()
                .map(|s| {
                    number(format!("slack.{}.thread_idle_days", s.workspace), s.thread_idle_days)
                })
                .collect()
        },
    },
    ConfigKey {
        path: "slack[].stale_after_hours",
        area: "connectors",
        class: EffectClass::Live,
        rows: |config| {
            config
                .slack
                .iter()
                .map(|s| {
                    number(format!("slack.{}.stale_after_hours", s.workspace), s.stale_after_hours)
                })
                .collect()
        },
    },
    ConfigKey {
        path: "systemone.enabled",
        area: "connectors",
        class: EffectClass::NextSession,
        rows: |config| {
            config.systemone.as_ref().map(|_| flag("systemone.enabled", true)).into_iter().collect()
        },
    },
    ConfigKey {
        path: "systemone.base_url",
        area: "connectors",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .systemone
                .as_ref()
                .map(|s| text("systemone.base_url", &s.base_url))
                .into_iter()
                .collect()
        },
    },
    ConfigKey {
        path: "systemone.api_key",
        area: "connectors",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .systemone
                .as_ref()
                .map(|s| {
                    text("systemone.api_key", s.api_key.clone().unwrap_or_else(|| "not set".into()))
                })
                .into_iter()
                .collect()
        },
    },
    ConfigKey {
        path: "systemone.model",
        area: "connectors",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .systemone
                .as_ref()
                .map(|s| text("systemone.model", &s.model))
                .into_iter()
                .collect()
        },
    },
    ConfigKey {
        path: "systemone.timeout_ms",
        area: "connectors",
        class: EffectClass::NextSession,
        rows: |config| {
            config
                .systemone
                .as_ref()
                .map(|s| number("systemone.timeout_ms", s.timeout_ms))
                .into_iter()
                .collect()
        },
    },
    ConfigKey {
        path: "plugins.auto_update",
        area: "connectors",
        class: EffectClass::NotReadYet,
        rows: |config| vec![flag("plugins.auto_update", config.plugins.auto_update)],
    },
];

const SERVER_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "server.enabled",
        area: "server",
        class: EffectClass::Restart,
        rows: |config| vec![flag("server.enabled", config.server.enabled)],
    },
    ConfigKey {
        path: "server.port",
        area: "server",
        class: EffectClass::Restart,
        rows: |config| vec![number("server.port", config.server.port)],
    },
    ConfigKey {
        path: "server.bind",
        area: "server",
        class: EffectClass::Restart,
        rows: |config| vec![text("server.bind", config.server.bind.to_string())],
    },
    ConfigKey {
        path: "server.first_frame_cap_kb",
        area: "server",
        class: EffectClass::Live,
        rows: |config| vec![number("server.first_frame_cap_kb", config.server.first_frame_cap_kb)],
    },
    ConfigKey {
        path: "server.tool_record_cap_kb",
        area: "server",
        class: EffectClass::Live,
        rows: |config| vec![number("server.tool_record_cap_kb", config.server.tool_record_cap_kb)],
    },
    ConfigKey {
        path: "browser.fps",
        area: "server",
        class: EffectClass::NextSession,
        rows: |config| vec![number("browser.fps", config.browser.fps)],
    },
    ConfigKey {
        path: "browser.quality",
        area: "server",
        class: EffectClass::NextSession,
        rows: |config| vec![number("browser.quality", config.browser.quality)],
    },
    ConfigKey {
        path: "browser.idle_minutes",
        area: "server",
        class: EffectClass::Live,
        rows: |config| vec![number("browser.idle_minutes", config.browser.idle_minutes)],
    },
];

fn enum_name<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

const CLIENT_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "client.mark",
        area: "client",
        class: EffectClass::Live,
        rows: |config| vec![text_set("client.mark", config.client.mark.clone())],
    },
    ConfigKey {
        path: "client.theme",
        area: "client",
        class: EffectClass::Live,
        rows: |config| vec![text_set("client.theme", config.client.theme.clone())],
    },
    ConfigKey {
        path: "client.font",
        area: "client",
        class: EffectClass::Live,
        rows: |config| vec![text_set("client.font", config.client.font.clone())],
    },
    ConfigKey {
        path: "dictate.enabled",
        area: "client",
        class: EffectClass::Restart,
        rows: |config| vec![flag("dictate.enabled", config.dictate.enabled)],
    },
    ConfigKey {
        path: "dictate.models_dir",
        area: "client",
        class: EffectClass::Restart,
        rows: |config| {
            vec![text(
                "dictate.models_dir",
                config.dictate.models_dir.clone().unwrap_or_else(|| "not set".to_owned()),
            )]
        },
    },
    ConfigKey {
        path: "dictate.transcribe_model",
        area: "client",
        class: EffectClass::Restart,
        rows: |config| {
            vec![text(
                "dictate.transcribe_model",
                config.dictate.transcribe_model.clone().unwrap_or_else(|| "not set".to_owned()),
            )]
        },
    },
    ConfigKey {
        path: "dictate.cleanup_model",
        area: "client",
        class: EffectClass::Restart,
        rows: |config| {
            vec![text(
                "dictate.cleanup_model",
                config.dictate.cleanup_model.clone().unwrap_or_else(|| "not set".to_owned()),
            )]
        },
    },
    ConfigKey {
        path: "dictate.language",
        area: "client",
        class: EffectClass::Restart,
        rows: |config| {
            vec![text(
                "dictate.language",
                config.dictate.language.clone().unwrap_or_else(|| "autodetect".to_owned()),
            )]
        },
    },
    ConfigKey {
        path: "dictate.normalizer",
        area: "client",
        class: EffectClass::Restart,
        rows: |config| vec![flag("dictate.normalizer", config.dictate.normalizer)],
    },
    ConfigKey {
        path: "dictate.max_capture_minutes",
        area: "client",
        class: EffectClass::Restart,
        rows: |config| {
            vec![number("dictate.max_capture_minutes", config.dictate.max_capture_minutes)]
        },
    },
    ConfigKey {
        path: "dictate.bind",
        area: "client",
        class: EffectClass::Live,
        rows: |config| vec![text("dictate.bind", config.dictate.bind.label())],
    },
    ConfigKey {
        path: "dictate.mode",
        area: "client",
        class: EffectClass::Live,
        rows: |config| vec![text("dictate.mode", config.dictate.mode.label())],
    },
    ConfigKey {
        path: "dictate.styling",
        area: "client",
        class: EffectClass::NextSession,
        rows: |config| {
            vec![text(
                "dictate.styling",
                config.dictate.styling.map_or_else(
                    || enum_name(forge_dictate::normalize::Styling::default()),
                    enum_name,
                ),
            )]
        },
    },
    ConfigKey {
        path: "dictate.structure",
        area: "client",
        class: EffectClass::NextSession,
        rows: |config| {
            vec![text(
                "dictate.structure",
                config.dictate.structure.map_or_else(
                    || enum_name(forge_dictate::normalize::Structure::default()),
                    enum_name,
                ),
            )]
        },
    },
    ConfigKey {
        path: "dictate.context",
        area: "client",
        class: EffectClass::NextSession,
        rows: |config| {
            vec![text(
                "dictate.context",
                config.dictate.context.map_or_else(
                    || enum_name(forge_dictate::normalize::Context::default()),
                    enum_name,
                ),
            )]
        },
    },
];
const MODEL_KEYS: &[ConfigKey] = &[ConfigKey {
    path: "models.*.auto_compact_window",
    area: "models",
    class: EffectClass::NextSession,
    rows: |config| {
        let mut names: Vec<&String> = config.models.keys().collect();
        names.sort_unstable();
        names
            .into_iter()
            .map(|name| {
                let path = format!("models.{name}.auto_compact_window");
                match config.models[name].auto_compact_window {
                    Some(window) => number(path, window),
                    None => text(path, "not set"),
                }
            })
            .collect()
    },
}];

const HANDOFF_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "handoff.enabled",
        area: "handoff",
        class: EffectClass::Live,
        rows: |config| vec![flag("handoff.enabled", config.handoff.enabled.unwrap_or(true))],
    },
    ConfigKey {
        path: "handoff.at_percent",
        area: "handoff",
        class: EffectClass::Live,
        rows: |config| {
            vec![number(
                "handoff.at_percent",
                config.handoff.at_percent.unwrap_or(u32::from(DEFAULT_HANDOFF_AT_PERCENT)),
            )]
        },
    },
    ConfigKey {
        path: "handoff.after_compactions",
        area: "handoff",
        class: EffectClass::Live,
        rows: |config| {
            vec![number(
                "handoff.after_compactions",
                config.handoff.after_compactions.unwrap_or(DEFAULT_HANDOFF_AFTER_COMPACTIONS),
            )]
        },
    },
];

const DOCUMENT_KEYS: &[ConfigKey] = &[ConfigKey {
    path: "documents.max_size_mib",
    area: "documents",
    class: EffectClass::Live,
    rows: |config| {
        vec![number("documents.max_size_mib", config.document_max_bytes / (1024 * 1024))]
    },
}];

const USAGE_KEYS: &[ConfigKey] = &[ConfigKey {
    path: "usage.poll_seconds",
    area: "usage",
    class: EffectClass::Live,
    rows: |config| vec![number("usage.poll_seconds", config.usage_poll_interval.as_secs())],
}];

const GATEWAY_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "gateway.port",
        area: "gateway",
        class: EffectClass::Restart,
        rows: |config| vec![number("gateway.port", config.gateway_port)],
    },
    ConfigKey {
        path: "gateway.streak_count",
        area: "gateway",
        class: EffectClass::Live,
        rows: |config| vec![number("gateway.streak_count", config.gateway_rotation.streak_count)],
    },
    ConfigKey {
        path: "gateway.streak_window_secs",
        area: "gateway",
        class: EffectClass::Live,
        rows: |config| {
            vec![number(
                "gateway.streak_window_secs",
                config.gateway_rotation.streak_window.as_secs(),
            )]
        },
    },
    ConfigKey {
        path: "gateway.no_reset_cooldown_secs",
        area: "gateway",
        class: EffectClass::Live,
        rows: |config| {
            vec![number(
                "gateway.no_reset_cooldown_secs",
                config.gateway_rotation.no_reset_cooldown.as_secs(),
            )]
        },
    },
];

const ENV_KEYS: &[ConfigKey] = &[ConfigKey {
    path: "env.*",
    area: "env",
    class: EffectClass::NextSession,
    rows: |config| {
        sorted_keys(&config.global_env)
            .into_iter()
            .map(|name| text(format!("env.{name}"), &config.global_env[name]))
            .collect()
    },
}];

/// The whole document for one load, every catalogued key resolved.
pub(crate) fn document(config: &LoadedConfig, version: &str, source: &Path) -> SettingsDocument {
    let areas = AREAS
        .iter()
        .map(|area| SettingsArea {
            id: area.id.to_owned(),
            name: area.name.to_owned(),
            note: area.note.to_owned(),
            keys: all_keys()
                .filter(|key| key.area == area.id)
                .flat_map(|key| {
                    (key.rows)(config).into_iter().map(|row| SettingsKey {
                        path: row.path,
                        kind: row.kind,
                        value: row.value,
                        class: key.class,
                    })
                })
                .collect(),
        })
        .collect();
    SettingsDocument {
        version: version.to_owned(),
        source: source.display().to_string(),
        totals: SettingsTotals { keys: 0, areas: 0 },
        areas,
    }
    .with_totals()
}

/// The reload summary, derived from the same rows the page draws: a key
/// reported as moved and the class the page chips it with are one table's.
pub(crate) fn diff(previous: &LoadedConfig, next: &LoadedConfig) -> ConfigReload {
    let mut reload = ConfigReload::default();
    for key in all_keys() {
        let old = (key.rows)(previous);
        let new = (key.rows)(next);
        for path in changed_paths(&old, &new) {
            match key.class {
                EffectClass::Restart => reload.restart_only.push(path),
                // Nothing reads it, so a change applies nothing and is not
                // reported as applied. The page still draws the new value.
                EffectClass::NotReadYet => {}
                EffectClass::Live | EffectClass::NextSession => reload.changed.push(path),
            }
        }
    }
    reload
}

fn changed_paths(old: &[KeyRow], new: &[KeyRow]) -> Vec<String> {
    let old_by_path: std::collections::HashMap<&str, &KeyRow> =
        old.iter().map(|row| (row.path.as_str(), row)).collect();
    let new_by_path: std::collections::HashMap<&str, &KeyRow> =
        new.iter().map(|row| (row.path.as_str(), row)).collect();
    let mut changed = Vec::new();
    for row in new {
        if old_by_path.get(row.path.as_str()) != Some(&row) {
            changed.push(row.path.clone());
        }
    }
    for row in old {
        if !new_by_path.contains_key(row.path.as_str()) {
            changed.push(row.path.clone());
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A config setting every key the loader accepts, so the catalogue can
    /// be walked against the loader itself rather than against a list that
    /// moves with it. A key added to a struct without a row here is what the
    /// completeness test below is for.
    const EVERY_KEY: &str = r#"
[env]
GLOBAL_FLAG = "1"

[[accounts]]
display_name = "Primary"
provider = "openrouter"
base_url = "https://openrouter.ai/api"
token = "t"
models = ["glm-5.3-flash"]
model_slugs = { "glm-5.3-flash" = "z-ai/glm-5.3-flash" }
model_aliases = { "glm-5.3-flash" = ["glm-flash"] }

[accounts.env]
PROVIDER_FLAG = "1"

[[orgs]]
name = "TestOrg"
accounts = ["Primary"]
fallback_accounts = ["Primary"]
browser_profile = "work"

[orgs.slack]
workspace = "Test Slack"

[[orgs.projects]]
name = "proj"
path = "/tmp/proj"
auto_start = true
model = "glm-5.3-flash"
permission_mode = "auto"
max_workers = 3
env_file = "/tmp/forge-settings-test.env"
vault_roots = ["/tmp"]
browser_profile = "projprofile"

[orgs.projects.env]
PROJECT_FLAG = "1"

[orgs.projects.handoff]
enabled = false
at_percent = 70
after_compactions = 2

[orgs.projects.slack]
workspace = "Test Slack"

[models."glm-5.3-flash"]
auto_compact_window = 1024

[handoff]
enabled = true
at_percent = 80
after_compactions = 3

[dictate]
enabled = true
models_dir = "/tmp/models"
transcribe_model = "m1"
cleanup_model = "m2"
language = "en"
normalizer = false
max_capture_minutes = 10
bind = "left_cmd"
mode = "hold"
styling = "casual"
structure = "lists"
context = "email"

[gotify]
url = "https://gotify.test"
client_token = "g"

[systemone]
enabled = true
base_url = "https://systemone.test"
api_key = "k"
model = "jev"
timeout_ms = 1000

[[slack]]
workspace = "Test Slack"
token = "x"
poll_seconds = 10
dm_poll_seconds = 60
thread_idle_days = 3
stale_after_hours = 6

[plugins]
auto_update = true

[gateway]
port = 8787
streak_count = 3
streak_window_secs = 30
no_reset_cooldown_secs = 20

[server]
enabled = true
port = 8790
bind = "127.0.0.1"
first_frame_cap_kb = 64
tool_record_cap_kb = 8

[client]
mark = "panes"
theme = "dark"
font = "system"

[browser]
fps = 10
quality = 50
idle_minutes = 5

[documents]
max_size_mib = 5

[usage]
poll_seconds = 5
"#;

    fn load(raw: &str) -> LoadedConfig {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        std::fs::write(forge.join("forge.toml"), raw).expect("write config");
        crate::config::load_from_dir(dir.path()).expect("the fixture loads")
    }

    /// The load refusal a probe config produces, without the tempdir naming
    /// anything the test reads back.
    fn load_error(raw: &str) -> crate::error::WorkspaceError {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        std::fs::write(forge.join("forge.toml"), raw).expect("write config");
        crate::config::load_from_dir(dir.path()).expect_err("the stray key is refused")
    }

    /// Every key the loader accepts, as the fixture's own leaf paths, is in
    /// the catalogue, and every catalogue entry is a key the fixture sets.
    /// A key accepted by a struct but missing here is the drift this catches
    /// only if the fixture moves with the struct; the round trip is what
    /// proves each catalogued path is really accepted.
    #[test]
    fn the_catalogue_is_the_loaders_key_set() {
        let value: toml::Value = toml::from_str(EVERY_KEY).expect("the fixture parses");
        let mut leaves = Vec::new();
        collect_leaves("", &value, &mut leaves);
        let fixture: BTreeSet<String> = leaves.iter().map(|path| normalize_path(path)).collect();
        let catalogue: BTreeSet<String> = all_keys().map(|key| key.path.to_owned()).collect();
        assert_eq!(
            fixture, catalogue,
            "a key the fixture sets has no catalogue row, or the catalogue names a key the \
             loader does not accept",
        );
    }

    /// The other direction, from the structs themselves: a stray key makes
    /// serde name every field it would accept, so a field added to a struct
    /// reaches this test without anyone remembering to add it to a list.
    #[test]
    fn the_catalogue_covers_every_field_the_loader_accepts() {
        const BASE: &str = r#"
[[orgs]]
name = "O"
accounts = ["A"]

[[orgs.projects]]
name = "P"
path = "/tmp/p"

[[accounts]]
display_name = "A"
provider = "anthropic"
token = "t"
models = ["m"]
"#;
        let top = accepted_fields(&format!("zzz_unknown = 1\n{BASE}"));
        let accounts = accepted_fields(&format!(
            "{BASE}\n[[accounts]]\nzzz_unknown = 1\ndisplay_name = \"B\"\nprovider = \
             \"anthropic\"\ntoken = \"t\"\nmodels = [\"m\"]\n"
        ));
        let orgs = accepted_fields(&format!(
            "{BASE}\n[[orgs]]\nzzz_unknown = 1\nname = \"O2\"\naccounts = [\"A\"]\n"
        ));
        let projects = accepted_fields(&format!(
            "{BASE}\n[[orgs]]\nname = \"O2\"\naccounts = [\"A\"]\n[[orgs.projects]]\n\
             zzz_unknown = 1\nname = \"P2\"\npath = \"/tmp/p2\"\n"
        ));
        let org_slack = accepted_fields(&format!(
            "{BASE}\n[[orgs]]\nname = \"O2\"\naccounts = [\"A\"]\n[orgs.slack]\nzzz_unknown = 1\n"
        ));
        let project_slack = accepted_fields(&format!(
            "{BASE}\n[[orgs]]\nname = \"O2\"\naccounts = [\"A\"]\n[[orgs.projects]]\nname = \
             \"P2\"\npath = \"/tmp/p2\"\n[orgs.projects.slack]\nzzz_unknown = 1\n"
        ));
        let project_handoff = accepted_fields(&format!(
            "{BASE}\n[[orgs]]\nname = \"O2\"\naccounts = [\"A\"]\n[[orgs.projects]]\nname = \
             \"P2\"\npath = \"/tmp/p2\"\n[orgs.projects.handoff]\nzzz_unknown = 1\n"
        ));
        let models = accepted_fields(&format!("{BASE}\n[models.\"m\"]\nzzz_unknown = 1\n"));
        let handoff = accepted_fields(&format!("{BASE}\n[handoff]\nzzz_unknown = 1\n"));
        let dictate = accepted_fields(&format!("{BASE}\n[dictate]\nzzz_unknown = 1\n"));
        let gotify = accepted_fields(&format!("{BASE}\n[gotify]\nzzz_unknown = 1\n"));
        let systemone = accepted_fields(&format!("{BASE}\n[systemone]\nzzz_unknown = 1\n"));
        let slack = accepted_fields(&format!("{BASE}\n[[slack]]\nzzz_unknown = 1\n"));
        let plugins = accepted_fields(&format!("{BASE}\n[plugins]\nzzz_unknown = 1\n"));
        let gateway = accepted_fields(&format!("{BASE}\n[gateway]\nzzz_unknown = 1\n"));
        let server = accepted_fields(&format!("{BASE}\n[server]\nzzz_unknown = 1\n"));
        let client = accepted_fields(&format!("{BASE}\n[client]\nzzz_unknown = 1\n"));
        let browser = accepted_fields(&format!("{BASE}\n[browser]\nzzz_unknown = 1\n"));
        let documents = accepted_fields(&format!("{BASE}\n[documents]\nzzz_unknown = 1\n"));
        let usage = accepted_fields(&format!("{BASE}\n[usage]\nzzz_unknown = 1\n"));

        assert_eq!(
            models.iter().map(String::as_str).collect::<Vec<_>>(),
            ["auto_compact_window"],
            "the model table's fields moved",
        );

        let mut accepted = BTreeSet::new();
        for field in &top {
            match field.as_str() {
                // The retired sections a stale file may still carry.
                "web" | "workers" | "projects" | "selection" | "ui" => {}
                "orgs" => accepted.extend(org_names_from(&orgs, &projects, &org_slack, &handoff)),
                "accounts" => accepted.extend(account_names_from(&accounts)),
                "models" => {
                    accepted.insert("models.*.auto_compact_window".to_owned());
                }
                "handoff" => accepted.extend(handoff_names(&handoff, "handoff.")),
                "dictate" => accepted.extend(
                    dictate
                        .iter()
                        .filter(|field| field.as_str() != "device")
                        .map(|field| format!("dictate.{field}")),
                ),
                "gotify" => accepted.extend(gotify.iter().map(|field| format!("gotify.{field}"))),
                "systemone" => {
                    accepted.extend(systemone.iter().map(|field| format!("systemone.{field}")));
                }
                "slack" => accepted.extend(slack_names(&slack)),
                "plugins" => {
                    accepted.extend(plugins.iter().map(|field| format!("plugins.{field}")));
                }
                "gateway" => {
                    accepted.extend(gateway.iter().map(|field| format!("gateway.{field}")));
                }
                "server" => accepted.extend(server.iter().map(|field| format!("server.{field}"))),
                "client" => accepted.extend(client.iter().map(|field| format!("client.{field}"))),
                "browser" => {
                    accepted.extend(browser.iter().map(|field| format!("browser.{field}")));
                }
                "documents" => {
                    accepted.extend(documents.iter().map(|field| format!("documents.{field}")));
                }
                "usage" => accepted.extend(usage.iter().map(|field| format!("usage.{field}"))),
                "env" => {
                    accepted.insert("env.*".to_owned());
                }
                other => panic!("no catalogue rule for the top-level field `{other}`"),
            }
        }
        let catalogue: BTreeSet<String> = all_keys().map(|key| key.path.to_owned()).collect();
        assert_eq!(
            accepted, catalogue,
            "a field the loader accepts has no catalogue row, or the catalogue names a key the \
             loader does not accept",
        );
        // The project probes are read through the org expansion above; this
        // names them so a later edit cannot drop those two probes silently.
        assert!(!project_slack.is_empty(), "the project slack probe named no fields");
        assert!(!project_handoff.is_empty(), "the project handoff probe named no fields");
    }

    fn accepted_fields(probe: &str) -> BTreeSet<String> {
        let error = load_error(probe);
        let message = error.to_string();
        let listed = message
            .split("expected ")
            .nth(1)
            .unwrap_or_else(|| panic!("serde named no accepted fields: {message}"));
        let listed = listed.strip_prefix("one of ").unwrap_or(listed);
        listed
            .split(", ")
            .flat_map(|field| field.split(" or "))
            .map(|field| field.trim().trim_matches('`').to_owned())
            .filter(|field| field != "zzz_unknown")
            .collect()
    }

    fn org_names_from(
        orgs: &BTreeSet<String>,
        projects: &BTreeSet<String>,
        org_slack: &BTreeSet<String>,
        handoff: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut names = Vec::new();
        for field in orgs {
            match field.as_str() {
                "slack" => {
                    names.extend(org_slack.iter().map(|field| format!("orgs[].slack.{field}")));
                }
                "projects" => names.extend(project_names(projects, org_slack, handoff)),
                other => names.push(format!("orgs[].{other}")),
            }
        }
        names
    }

    /// A project's fields, with its own nested tables expanded by the
    /// probes taken at `[orgs.projects.slack]` and
    /// `[orgs.projects.handoff]`.
    fn project_names(
        projects: &BTreeSet<String>,
        project_slack: &BTreeSet<String>,
        handoff: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut names = Vec::new();
        for field in projects {
            match field.as_str() {
                "env" => names.push("orgs[].projects[].env.*".to_owned()),
                "slack" => names.extend(
                    project_slack.iter().map(|field| format!("orgs[].projects[].slack.{field}")),
                ),
                "handoff" => names.extend(handoff_names(handoff, "orgs[].projects[].handoff.")),
                other => names.push(format!("orgs[].projects[].{other}")),
            }
        }
        names
    }

    fn account_names_from(accounts: &BTreeSet<String>) -> Vec<String> {
        accounts
            .iter()
            .map(|field| match field.as_str() {
                "env" | "model_slugs" | "model_aliases" => format!("accounts[].{field}.*"),
                other => format!("accounts[].{other}"),
            })
            .collect()
    }

    fn handoff_names(handoff: &BTreeSet<String>, prefix: &str) -> Vec<String> {
        handoff.iter().map(|field| format!("{prefix}{field}")).collect()
    }

    fn slack_names(slack: &BTreeSet<String>) -> Vec<String> {
        slack.iter().map(|field| format!("slack[].{field}")).collect()
    }

    /// Each class the page can draw has at least one key, so a class cannot
    /// become a legend entry nothing ever wears.
    #[test]
    fn every_effect_class_has_a_key() {
        for class in [
            EffectClass::Live,
            EffectClass::NextSession,
            EffectClass::Restart,
            EffectClass::NotReadYet,
        ] {
            assert!(all_keys().any(|key| key.class == class), "no key carries {class:?}");
        }
    }

    /// Every catalogue entry resolves a row from the every-key fixture, so a
    /// resolver that forgot its value cannot pass as a drawn key.
    #[test]
    fn every_catalogued_key_resolves_a_row() {
        let config = load(EVERY_KEY);
        for key in all_keys() {
            assert!(
                !(key.rows)(&config).is_empty(),
                "{} resolved no row from a config that sets it",
                key.path,
            );
        }
    }

    /// The document covers the eleven areas and counts what it draws.
    #[test]
    fn the_document_covers_every_area_and_counts_its_keys() {
        let config = load(EVERY_KEY);
        let document = document(&config, "1.2.3", Path::new("/tmp/forge.toml"));
        assert_eq!(document.areas.len(), AREAS.len(), "every area is drawn");
        assert_eq!(document.areas.len(), 11, "the sub-nav's eleven areas");
        assert_eq!(
            document.totals.keys,
            document.areas.iter().map(|area| area.keys.len()).sum::<usize>(),
            "the header's count is what the areas hold",
        );
        for area in &document.areas {
            assert!(!area.keys.is_empty(), "area {} drew no key", area.id);
        }
        assert_eq!(document.version, "1.2.3");
        assert_eq!(document.source, "/tmp/forge.toml");
    }

    /// An axis the file does not set crosses as `null`, not as the display
    /// default: the page draws the built-in, and the client's axes path never
    /// applies a name it would resolve to nothing.
    #[test]
    fn an_unset_client_axis_is_null_in_the_document() {
        let raw = EVERY_KEY
            .replace("[client]\nmark = \"panes\"\ntheme = \"dark\"\nfont = \"system\"\n", "");
        assert!(!raw.contains("[client]"), "precondition: the client table was removed");
        let config = load(&raw);
        let document = document(&config, "v", Path::new("/tmp/forge.toml"));
        let client = document
            .areas
            .iter()
            .find(|area| area.id == "client")
            .expect("the client area is drawn");
        for path in ["client.mark", "client.theme", "client.font"] {
            let key = client
                .keys
                .iter()
                .find(|key| key.path == path)
                .unwrap_or_else(|| panic!("{path} has no row"));
            assert_eq!(
                key.value,
                serde_json::Value::Null,
                "{path} must carry the raw unset value, not a display default",
            );
        }
    }

    /// A class split inside one table stays split in the reload summary: a
    /// live cap is not reported as a restart, and a port is not reported as
    /// live.
    #[test]
    fn the_reload_reports_each_key_by_its_own_class() {
        let previous = load(EVERY_KEY);
        let changed = EVERY_KEY
            .replace("first_frame_cap_kb = 64", "first_frame_cap_kb = 128")
            .replace("port = 8790", "port = 8791")
            .replace("auto_update = true", "auto_update = false");
        let next = load(&changed);
        let reload = diff(&previous, &next);
        assert!(
            reload.changed.iter().any(|key| key == "server.first_frame_cap_kb"),
            "the cap is live: {reload:?}",
        );
        assert!(
            reload.restart_only.iter().any(|key| key == "server.port"),
            "the port needs a restart: {reload:?}",
        );
        assert!(
            !reload.changed.iter().any(|key| key == "server.port"),
            "the port is not live: {reload:?}",
        );
        assert!(
            !reload.changed.iter().any(|key| key.contains("plugins")),
            "a key nothing reads is not reported as applied: {reload:?}",
        );
    }

    /// A key nothing reads still changes the document, which is what makes
    /// the page show its new value even though the reload applies nothing.
    #[test]
    fn an_unread_key_changes_the_document_but_not_the_reload() {
        let previous = load(EVERY_KEY);
        let next = load(&EVERY_KEY.replace("auto_update = true", "auto_update = false"));
        let reload = diff(&previous, &next);
        assert!(reload.changed.is_empty(), "{reload:?}");
        assert!(reload.restart_only.is_empty(), "{reload:?}");
        assert_ne!(
            document(&previous, "v", Path::new("/tmp/forge.toml")),
            document(&next, "v", Path::new("/tmp/forge.toml")),
            "the page's value moved even though nothing applies it",
        );
    }

    /// A config that did not move draws the same document, so a reload can
    /// skip the push.
    #[test]
    fn an_unchanged_file_draws_an_equal_document() {
        let previous = load(EVERY_KEY);
        let next = load(EVERY_KEY);
        assert_eq!(
            document(&previous, "v", Path::new("/tmp/forge.toml")),
            document(&next, "v", Path::new("/tmp/forge.toml")),
        );
    }

    /// Keys may hold dots (`glm-5.3-flash`), so the walk separates
    /// segments with a character no key carries.
    const SEP: char = '\u{1f}';

    fn collect_leaves(prefix: &str, value: &toml::Value, out: &mut Vec<String>) {
        match value {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    let path =
                        if prefix.is_empty() { key.clone() } else { format!("{prefix}{SEP}{key}") };
                    collect_leaves(&path, value, out);
                }
            }
            toml::Value::Array(items) if items.iter().all(toml::Value::is_table) => {
                for item in items {
                    collect_leaves(&format!("{prefix}{SEP}[]"), item, out);
                }
            }
            _ => out.push(prefix.to_owned()),
        }
    }

    fn normalize_path(path: &str) -> String {
        let segments: Vec<&str> = path.split(SEP).collect();
        let mut out = String::new();
        for (index, segment) in segments.iter().enumerate() {
            let previous = index.checked_sub(1).map_or("", |i| segments[i]);
            let before = index.checked_sub(2).map_or("", |i| segments[i]);
            let wildcard = previous == "env"
                || (previous == "[]" && before == "env")
                || previous == "model_slugs"
                || previous == "model_aliases"
                || (index == 1 && segments[0] == "models");
            let value = if wildcard { "*" } else { *segment };
            if value == "[]" {
                out.push_str("[]");
            } else if out.is_empty() {
                out.push_str(value);
            } else {
                out.push('.');
                out.push_str(value);
            }
        }
        out
    }

    #[test]
    fn the_path_normaliser_folds_instances_into_their_pattern() {
        assert_eq!(normalize_path("accounts\u{1f}[]\u{1f}token"), "accounts[].token");
        assert_eq!(
            normalize_path("orgs\u{1f}[]\u{1f}projects\u{1f}[]\u{1f}env\u{1f}X"),
            "orgs[].projects[].env.*",
        );
        assert_eq!(
            normalize_path("models\u{1f}m\u{1f}auto_compact_window"),
            "models.*.auto_compact_window",
        );
        assert_eq!(
            normalize_path("accounts\u{1f}[]\u{1f}model_slugs\u{1f}glm-5.3-flash"),
            "accounts[].model_slugs.*",
        );
        assert_eq!(normalize_path("slack\u{1f}[]\u{1f}workspace"), "slack[].workspace");
    }

    /// The fixture's own directory cannot outlive the test's config load, so
    /// the path is only ever read at load. This keeps `PathBuf` imported for
    /// the source assertions above where a path is built.
    #[test]
    fn the_source_path_is_the_one_the_builder_was_handed() {
        let config = load(EVERY_KEY);
        let source = PathBuf::from("/tmp/forge.toml");
        let document = document(&config, "v", &source);
        assert_eq!(document.source, source.display().to_string());
    }
}
