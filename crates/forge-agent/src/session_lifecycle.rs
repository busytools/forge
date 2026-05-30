//! Model resolution + connect-event + initialize-result mapping.
//! Mirrors the relevant portions of upstream's
//! `agent-sdk/src/bridge/session_lifecycle.ts`.
//!
//! What lives here:
//! - `normalize_model_key` + `humanize_model_id`
//! - `model_keys_are_compatible` / `same_context_suffix` / `same_family_and_version`
//!   / `has_variant_sibling_conflict`
//! - `resolve_catalog_model` / `resolve_current_model`
//! - `current_model_is_authoritative`
//! - `current_models_equal` / `refresh_current_model`
//! - `map_available_models` (initialize-response `models` array → typed)
//!
//! What does NOT live here (despite upstream parking it in
//! `session_lifecycle.ts`): mode list logic (see `commands.rs`), MCP
//! reconciliation (see `mcp.rs`), `AskUserQuestion` sequencing (see
//! `user_interaction.rs`), the bulk of permission/elicitation routing.

use serde_json::Value;

use forge_primitives::{AvailableModel, CurrentModel, EffortLevel};

const OPUS_MODEL_ALIAS: &str = "opus";
const MAX_MODEL_VERSION_PARTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    Opus,
    Sonnet,
    Haiku,
    Unknown,
}

#[derive(Debug, Clone)]
struct NormalizedModelKey {
    original: String,
    family: ModelFamily,
    version_parts: Vec<u32>,
    variant_parts: Vec<String>,
    /// Lowercased; empty when the id has no `[...]` suffix.
    context_suffix: String,
}

fn normalize_model_key(id: &str) -> NormalizedModelKey {
    let original = id.trim().to_owned();
    if original.is_empty() {
        return NormalizedModelKey {
            original,
            family: ModelFamily::Unknown,
            version_parts: Vec::new(),
            variant_parts: Vec::new(),
            context_suffix: String::new(),
        };
    }
    let lower = original.to_ascii_lowercase();
    let (without_context, context_suffix) = if let Some(open) = lower.rfind('[')
        && lower.ends_with(']')
    {
        (lower[..open].to_owned(), lower[open + 1..lower.len() - 1].to_owned())
    } else {
        (lower.clone(), String::new())
    };
    let trimmed_pre = without_context.trim_end_matches('-');
    let without_prefix = trimmed_pre.strip_prefix("claude-").unwrap_or(trimmed_pre);
    let mut parts = without_prefix.split('-').filter(|p| !p.is_empty());
    let family_part = parts.next().unwrap_or("");
    let family = match family_part {
        "opus" => ModelFamily::Opus,
        "sonnet" => ModelFamily::Sonnet,
        "haiku" => ModelFamily::Haiku,
        _ => ModelFamily::Unknown,
    };

    let mut version_parts: Vec<u32> = Vec::new();
    let mut variant_parts: Vec<String> = Vec::new();

    if family != ModelFamily::Unknown {
        for part in parts {
            if part.chars().all(|c| c.is_ascii_digit()) {
                if version_parts.len() < MAX_MODEL_VERSION_PARTS {
                    if let Ok(v) = part.parse::<u32>() {
                        version_parts.push(v);
                    }
                    continue;
                }
                // 8-digit `20YYMMDD` release-build token  -  upstream
                // tracks these in `build_parts` but never surfaces them
                // in the user-visible humanizer, so drop.
                if part.len() == 8
                    && part.starts_with("20")
                    && part.chars().all(|c| c.is_ascii_digit())
                {
                    continue;
                }
            }
            variant_parts.push(part.to_owned());
        }
    }

    NormalizedModelKey { original, family, version_parts, variant_parts, context_suffix }
}

fn family_label(family: ModelFamily) -> Option<&'static str> {
    match family {
        ModelFamily::Opus => Some("Opus"),
        ModelFamily::Sonnet => Some("Sonnet"),
        ModelFamily::Haiku => Some("Haiku"),
        ModelFamily::Unknown => None,
    }
}

fn format_humanized(key: &NormalizedModelKey) -> String {
    let Some(family_lbl) = family_label(key.family) else {
        return key.original.clone();
    };
    let version_lbl = if key.version_parts.is_empty() {
        String::new()
    } else {
        format!(" {}", key.version_parts.iter().map(u32::to_string).collect::<Vec<_>>().join("."))
    };
    let context_lbl = match key.context_suffix.as_str() {
        "" => String::new(),
        s if s.eq_ignore_ascii_case("1m") => " [1M]".to_owned(),
        s => format!(" [{s}]"),
    };
    format!("{family_lbl}{version_lbl}{context_lbl}")
}

fn humanize_model_id(id: &str) -> String {
    format_humanized(&normalize_model_key(id))
}

fn model_keys_are_compatible(left_id: &str, right_id: &str) -> bool {
    let left = normalize_model_key(left_id);
    let right = normalize_model_key(right_id);
    if left.family == ModelFamily::Unknown || right.family == ModelFamily::Unknown {
        return left.original.eq_ignore_ascii_case(&right.original);
    }
    if left.family != right.family {
        return false;
    }
    if left.variant_parts.join(".") != right.variant_parts.join(".") {
        return false;
    }
    if left.version_parts.is_empty() || right.version_parts.is_empty() {
        return true;
    }
    left.version_parts == right.version_parts
}

fn same_context_suffix(left_id: &str, right_id: &str) -> bool {
    normalize_model_key(left_id).context_suffix == normalize_model_key(right_id).context_suffix
}

fn same_family_and_version(left_id: &str, right_id: &str) -> bool {
    let left = normalize_model_key(left_id);
    let right = normalize_model_key(right_id);
    if left.family == ModelFamily::Unknown || right.family == ModelFamily::Unknown {
        return left.original.eq_ignore_ascii_case(&right.original);
    }
    if left.family != right.family {
        return false;
    }
    if left.version_parts.is_empty() || right.version_parts.is_empty() {
        return left.version_parts.len() == right.version_parts.len();
    }
    left.version_parts == right.version_parts
}

fn has_variant_sibling_conflict(
    available_models: &[AvailableModel],
    candidate_id: &str,
    resolved_id: &str,
) -> bool {
    if same_context_suffix(candidate_id, resolved_id) {
        return false;
    }
    let resolved_context = normalize_model_key(resolved_id).context_suffix;
    if resolved_context.is_empty() {
        return false;
    }
    available_models.iter().any(|entry| {
        if entry.id == candidate_id {
            return false;
        }
        if !same_family_and_version(&entry.id, resolved_id) {
            return false;
        }
        normalize_model_key(&entry.id).context_suffix == resolved_context
    })
}

fn current_model_is_authoritative(resolved_id: &str, requested_id: Option<&str>) -> bool {
    let resolved = resolved_id.trim();
    if resolved.is_empty() || resolved == "Connecting..." {
        return requested_id.is_some_and(|s| !s.trim().is_empty());
    }
    true
}

fn resolve_catalog_model<'a>(
    available_models: &'a [AvailableModel],
    resolved_id: &str,
    requested_id: Option<&str>,
) -> Option<&'a AvailableModel> {
    if let Some(exact) = available_models.iter().find(|m| m.id == resolved_id) {
        return Some(exact);
    }
    if let Some(req) = requested_id
        && let Some(exact_requested) = available_models.iter().find(|m| m.id == req)
        && model_keys_are_compatible(&exact_requested.id, resolved_id)
        && !has_variant_sibling_conflict(available_models, &exact_requested.id, resolved_id)
    {
        return Some(exact_requested);
    }
    let compatible: Vec<&AvailableModel> = available_models
        .iter()
        .filter(|m| {
            model_keys_are_compatible(&m.id, resolved_id)
                && !has_variant_sibling_conflict(available_models, &m.id, resolved_id)
        })
        .collect();
    if compatible.len() == 1 {
        return Some(compatible[0]);
    }
    None
}

/// Mirrors upstream's `resolveCurrentModel`. Pure function on the
/// session's resolved/runtime/requested model strings + the
/// `available_models` catalogue. Primitive-arg form  -  callers pass
/// what they have; no session struct dependency.
pub fn resolve_current_model_from_inputs(
    model_id: &str,
    requested_model_id: Option<&str>,
    resolved_runtime_model_id: Option<&str>,
    available_models: &[AvailableModel],
) -> CurrentModel {
    let requested_id = requested_model_id.map(str::trim).filter(|s| !s.is_empty());
    let resolved_id =
        resolved_runtime_model_id.map(str::trim).filter(|s| !s.is_empty()).map_or_else(
            || {
                if model_id.trim().is_empty() {
                    requested_id.unwrap_or(OPUS_MODEL_ALIAS).to_owned()
                } else {
                    model_id.trim().to_owned()
                }
            },
            str::to_owned,
        );

    let catalog = resolve_catalog_model(available_models, &resolved_id, requested_id);
    let runtime_display_id = if resolved_id.is_empty() {
        requested_id.unwrap_or(OPUS_MODEL_ALIAS)
    } else {
        resolved_id.as_str()
    };

    // Prefer the catalogue's `display_name` when it carries version
    // info (e.g. "Claude Opus 4.7"). Recent claude CLI builds ship
    // a short displayName ("Opus") without a version number, so when
    // the catalogue value lacks any digit we fall back to
    // humanize_model_id which parses the version off the resolved id
    // (e.g. "claude-opus-4-7-20251022" -> "Opus 4.7").
    let catalog_name = catalog.map(|m| m.display_name.clone()).filter(|s| !s.is_empty());
    let catalog_has_version =
        catalog_name.as_deref().is_some_and(|s| s.chars().any(|c| c.is_ascii_digit()));
    let display_name = if catalog_has_version {
        catalog_name.unwrap_or_else(|| humanize_model_id(runtime_display_id))
    } else {
        let humanized = humanize_model_id(runtime_display_id);
        let humanized_has_version = humanized.chars().any(|c| c.is_ascii_digit());
        if humanized_has_version { humanized } else { catalog_name.unwrap_or(humanized) }
    };

    CurrentModel {
        requested_id: requested_id.map(str::to_owned),
        resolved_id: resolved_id.clone(),
        display_name_short: display_name.clone(),
        display_name_long: display_name,
        catalog_id: catalog.map(|m| m.id.clone()),
        supports_effort: catalog.is_some_and(|m| m.supports_effort),
        supported_effort_levels: catalog
            .map_or_else(Vec::new, |m| m.supported_effort_levels.clone()),
        supports_fast_mode: catalog.and_then(|m| m.supports_fast_mode),
        supports_auto_mode: catalog.and_then(|m| m.supports_auto_mode),
        supports_adaptive_thinking: catalog.and_then(|m| m.supports_adaptive_thinking),
        is_authoritative: current_model_is_authoritative(&resolved_id, requested_id),
    }
}

/// Mirrors `mapAvailableModels(models)`  -  initialize-response `models`
/// array → typed `AvailableModel`. Drops entries lacking a non-empty
/// `value` or `displayName`.
pub fn map_available_models(models: Option<&Value>) -> Vec<AvailableModel> {
    let Some(arr) = models.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            let r = entry.as_object()?;
            let id = r.get("value").and_then(Value::as_str)?.trim().to_owned();
            if id.is_empty() {
                return None;
            }
            let display_name = r.get("displayName").and_then(Value::as_str)?.trim().to_owned();
            if display_name.is_empty() {
                return None;
            }
            let supported_effort_levels: Vec<EffortLevel> = r
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|l| match l.as_str()? {
                            "low" => Some(EffortLevel::Low),
                            "medium" => Some(EffortLevel::Medium),
                            "high" => Some(EffortLevel::High),
                            "xhigh" => Some(EffortLevel::Xhigh),
                            "max" => Some(EffortLevel::Max),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(AvailableModel {
                id,
                display_name,
                description: r.get("description").and_then(Value::as_str).map(str::to_owned),
                supports_effort: r.get("supportsEffort").and_then(Value::as_bool).unwrap_or(false),
                supported_effort_levels,
                supports_adaptive_thinking: r
                    .get("supportsAdaptiveThinking")
                    .and_then(Value::as_bool),
                supports_fast_mode: r.get("supportsFastMode").and_then(Value::as_bool),
                supports_auto_mode: r.get("supportsAutoMode").and_then(Value::as_bool),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_known_families() {
        assert_eq!(humanize_model_id("claude-sonnet-4-6"), "Sonnet 4.6");
        assert_eq!(humanize_model_id("claude-opus-4-7"), "Opus 4.7");
        assert_eq!(humanize_model_id("claude-haiku-4-5"), "Haiku 4.5");
    }

    #[test]
    fn humanize_with_context_suffix() {
        assert_eq!(humanize_model_id("claude-sonnet-4-6[1m]"), "Sonnet 4.6 [1M]");
        assert_eq!(humanize_model_id("claude-sonnet-4-6[8k]"), "Sonnet 4.6 [8k]");
    }

    #[test]
    fn humanize_unknown_falls_through() {
        assert_eq!(humanize_model_id("custom-model-1"), "custom-model-1");
        assert_eq!(humanize_model_id(""), "");
    }

    #[test]
    fn humanize_strips_release_build_tokens() {
        // build tokens are dropped from version_parts after MAX=2
        assert_eq!(humanize_model_id("claude-sonnet-4-6-20260101"), "Sonnet 4.6");
    }

    #[test]
    fn map_models_filters_empty_and_invalid() {
        let v = serde_json::json!([
            { "value": "a", "displayName": "A", "supportsEffort": true,
              "supportedEffortLevels": ["low", "medium", "wrong"] },
            { "value": "  ", "displayName": "skip" },
            { "value": "b", "displayName": "  " },
            { "value": "c", "displayName": "C" },
        ]);
        let m = map_available_models(Some(&v));
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "a");
        assert_eq!(m[0].supported_effort_levels.len(), 2);
        assert_eq!(m[1].id, "c");
    }

    #[test]
    fn resolve_current_model_falls_back_to_opus() {
        let cm = resolve_current_model_from_inputs("", None, None, &[]);
        assert_eq!(cm.resolved_id, "opus");
        // Matches upstream: non-empty resolved id (even the "opus"
        // alias) is treated as authoritative.
        assert!(cm.is_authoritative);

        let cm = resolve_current_model_from_inputs("claude-sonnet-4-6", None, None, &[]);
        assert_eq!(cm.resolved_id, "claude-sonnet-4-6");
        assert_eq!(cm.display_name_short, "Sonnet 4.6");
        assert!(cm.is_authoritative);
    }

    #[test]
    fn catalog_short_name_augmented_with_version_from_resolved_id() {
        // Recent claude CLI builds ship `displayName: "Opus"` without a
        // version number. When the resolved id carries version info,
        // prefer humanize_model_id so the display name renders as
        // "Opus 4.7" rather than the bare family name.
        let catalog = vec![AvailableModel {
            id: "claude-opus-4-7-20251022".to_owned(),
            display_name: "Opus".to_owned(),
            description: None,
            supports_effort: false,
            supported_effort_levels: vec![],
            supports_adaptive_thinking: None,
            supports_fast_mode: None,
            supports_auto_mode: None,
        }];
        let cm =
            resolve_current_model_from_inputs("claude-opus-4-7-20251022", None, None, &catalog);
        assert_eq!(cm.display_name_short, "Opus 4.7");
        assert_eq!(cm.display_name_long, "Opus 4.7");
    }

    #[test]
    fn catalog_versioned_name_kept_as_is() {
        // When the catalog entry already has a version, keep it
        // verbatim  -  don't second-guess the CLI.
        let catalog = vec![AvailableModel {
            id: "claude-opus-4-7-20251022".to_owned(),
            display_name: "Claude Opus 4.7".to_owned(),
            description: None,
            supports_effort: false,
            supported_effort_levels: vec![],
            supports_adaptive_thinking: None,
            supports_fast_mode: None,
            supports_auto_mode: None,
        }];
        let cm =
            resolve_current_model_from_inputs("claude-opus-4-7-20251022", None, None, &catalog);
        assert_eq!(cm.display_name_short, "Claude Opus 4.7");
    }

    #[test]
    fn catalog_match_populates_supports_effort() {
        let catalog = vec![AvailableModel {
            id: "claude-sonnet-4-6".to_owned(),
            display_name: "Sonnet 4.6".to_owned(),
            description: None,
            supports_effort: true,
            supported_effort_levels: vec![EffortLevel::Medium, EffortLevel::High],
            supports_adaptive_thinking: Some(true),
            supports_fast_mode: Some(false),
            supports_auto_mode: Some(true),
        }];
        let cm = resolve_current_model_from_inputs("claude-sonnet-4-6", None, None, &catalog);
        assert!(cm.supports_effort);
        assert_eq!(cm.supported_effort_levels.len(), 2);
        assert_eq!(cm.catalog_id.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(cm.supports_auto_mode, Some(true));
    }
}
