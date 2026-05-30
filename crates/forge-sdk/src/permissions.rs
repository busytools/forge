//! Permission callback trait + tests. Wire-data lives in
//! forge-primitives - consumers import from there directly.

use forge_primitives::{PermissionDecision, ToolPermissionContext};

/// Trait for permission callbacks.
///
/// Implementations are typically async closures or plain functions. The
/// SDK wraps them in a boxed trait object inside `Options`.
///
/// # Panics and error handling
///
/// Callbacks are expected to never panic. If a callback does panic, the
/// `tokio` task running the current `next_event` call is aborted; the next
/// call to `next_event` returns an [`Error::Io`](crate::Error::Io) with a
/// broken-pipe or similar message. Authors should return
/// [`PermissionDecision::deny`] to signal rejection rather than panicking.
///
/// Callbacks cannot signal I/O or other errors. If your callback performs
/// fallible work (e.g., consulting a policy server), handle the failure
/// internally and translate to `allow` or `deny(reason)` - the SDK does not
/// surface callback errors to the `claude` binary separately from a deny
/// response.
pub trait CanUseToolCallback: Send + Sync {
    /// Called by the SDK when the `claude` binary requests permission.
    fn call<'a>(
        &'a self,
        ctx: ToolPermissionContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>>;
}

impl<F, Fut> CanUseToolCallback for F
where
    F: Fn(ToolPermissionContext) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = PermissionDecision> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        ctx: ToolPermissionContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>> {
        Box::pin(self(ctx))
    }
}

#[cfg(test)]
mod tests {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use forge_primitives::{PermissionDecision, ToolPermissionContext};
    use serde_json::json;

    #[test]
    fn allow_decision_no_modifications() {
        let d = PermissionDecision::allow();
        assert!(d.is_allow());
        assert!(d.updated_input().is_none());
        assert!(d.reason().is_none());
    }

    #[test]
    fn allow_decision_with_updated_input() {
        let d = PermissionDecision::allow_with_input(json!({"file_path": "/tmp/safe.txt"}));
        assert!(d.is_allow());
        assert_eq!(d.updated_input().unwrap(), &json!({"file_path": "/tmp/safe.txt"}));
    }

    #[test]
    fn deny_decision_carries_reason() {
        let d = PermissionDecision::deny("not today");
        assert!(!d.is_allow());
        assert_eq!(d.reason(), Some("not today"));
    }

    #[test]
    fn context_carries_tool_name_and_input() {
        let ctx =
            ToolPermissionContext::new("Edit", json!({"file_path": "/tmp/x"}), "toolu_01", None);
        assert_eq!(ctx.tool_name, "Edit");
        assert_eq!(ctx.tool_use_id, "toolu_01");
    }

    use crate::PermissionMode;
    use forge_primitives::{
        PermissionBehavior, PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination,
    };

    #[test]
    fn add_rules_serialises_with_camel_case_wire_keys() {
        let update = PermissionUpdate::AddRules {
            rules: vec![
                PermissionRuleValue { tool_name: "Edit".into(), rule_content: Some("*.py".into()) },
                PermissionRuleValue { tool_name: "Bash".into(), rule_content: None },
            ],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::ProjectSettings),
        };
        let v = serde_json::to_value(&update).expect("ser");
        assert_eq!(
            v,
            json!({
                "type": "addRules",
                "rules": [
                    {"toolName": "Edit", "ruleContent": "*.py"},
                    {"toolName": "Bash"}
                ],
                "behavior": "allow",
                "destination": "projectSettings"
            })
        );
    }

    #[test]
    fn replace_and_remove_rules_variants_roundtrip() {
        for (variant, wire_type) in [
            (
                PermissionUpdate::ReplaceRules {
                    rules: vec![PermissionRuleValue {
                        tool_name: "Edit".into(),
                        rule_content: None,
                    }],
                    behavior: PermissionBehavior::Deny,
                    destination: None,
                },
                "replaceRules",
            ),
            (
                PermissionUpdate::RemoveRules {
                    rules: vec![PermissionRuleValue {
                        tool_name: "Bash".into(),
                        rule_content: Some("rm *".into()),
                    }],
                    behavior: PermissionBehavior::Ask,
                    destination: Some(PermissionUpdateDestination::Session),
                },
                "removeRules",
            ),
        ] {
            let v = serde_json::to_value(&variant).expect("ser");
            assert_eq!(v["type"], wire_type);
            let back: PermissionUpdate = serde_json::from_value(v).expect("de");
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(&variant).unwrap()
            );
        }
    }

    #[test]
    fn set_mode_serialises_with_camel_case_mode_string() {
        let update = PermissionUpdate::SetMode {
            mode: PermissionMode::AcceptEdits,
            destination: Some(PermissionUpdateDestination::UserSettings),
        };
        let v = serde_json::to_value(&update).expect("ser");
        assert_eq!(
            v,
            json!({
                "type": "setMode",
                "mode": "acceptEdits",
                "destination": "userSettings"
            })
        );
    }

    #[test]
    fn directory_variants_roundtrip() {
        let add = PermissionUpdate::AddDirectories {
            directories: vec!["/tmp/a".into(), "/tmp/b".into()],
            destination: None,
        };
        let v = serde_json::to_value(&add).expect("ser");
        assert_eq!(
            v,
            json!({
                "type": "addDirectories",
                "directories": ["/tmp/a", "/tmp/b"]
            })
        );

        let rem = PermissionUpdate::RemoveDirectories {
            directories: vec!["/tmp/a".into()],
            destination: Some(PermissionUpdateDestination::LocalSettings),
        };
        let v = serde_json::to_value(&rem).expect("ser");
        assert_eq!(v["type"], "removeDirectories");
        assert_eq!(v["destination"], "localSettings");
    }

    #[test]
    fn permission_behavior_values_match_wire() {
        for (b, wire) in [
            (PermissionBehavior::Allow, "allow"),
            (PermissionBehavior::Deny, "deny"),
            (PermissionBehavior::Ask, "ask"),
        ] {
            assert_eq!(serde_json::to_value(b).unwrap(), json!(wire));
        }
    }

    #[test]
    fn permission_update_destination_values_match_wire() {
        for (d, wire) in [
            (PermissionUpdateDestination::UserSettings, "userSettings"),
            (PermissionUpdateDestination::ProjectSettings, "projectSettings"),
            (PermissionUpdateDestination::LocalSettings, "localSettings"),
            (PermissionUpdateDestination::Session, "session"),
        ] {
            assert_eq!(serde_json::to_value(d).unwrap(), json!(wire));
        }
    }

    #[test]
    fn decision_allow_has_empty_permissions_by_default() {
        let d = PermissionDecision::allow();
        assert!(d.updated_permissions().is_empty());
    }

    #[test]
    fn decision_allow_with_input_has_empty_permissions_by_default() {
        let d = PermissionDecision::allow_with_input(json!({"foo": "bar"}));
        assert!(d.updated_permissions().is_empty());
        assert_eq!(d.updated_input(), Some(&json!({"foo": "bar"})));
    }

    #[test]
    fn with_updated_permissions_attaches_updates() {
        let updates = vec![
            PermissionUpdate::SetMode { mode: PermissionMode::Plan, destination: None },
            PermissionUpdate::AddDirectories {
                directories: vec!["/workspace".into()],
                destination: None,
            },
        ];
        let d = PermissionDecision::allow().with_updated_permissions(updates.clone());
        let got = d.updated_permissions();
        assert_eq!(got.len(), 2);
        assert_eq!(serde_json::to_value(got).unwrap(), serde_json::to_value(&updates).unwrap());
    }

    #[test]
    fn with_updated_permissions_is_noop_on_deny() {
        let d = PermissionDecision::deny("nope").with_updated_permissions(vec![
            PermissionUpdate::SetMode { mode: PermissionMode::Ask, destination: None },
        ]);
        // Still a deny; permissions are not readable.
        assert!(d.updated_permissions().is_empty());
        assert_eq!(d.reason(), Some("nope"));
    }
}
