//! Round-trip tests for `PermissionUpdate` and its attachment to
//! `PermissionDecision::allow_with_input`.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:102-170`. The wire
//! shape is verified by serialising each variant and asserting on the
//! resulting JSON matches Python's `PermissionUpdate.to_dict()` output.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::PermissionMode;
use forge_sdk::{
    PermissionBehavior, PermissionDecision, PermissionRuleValue, PermissionUpdate,
    PermissionUpdateDestination,
};
use serde_json::json;

#[test]
fn add_rules_serialises_with_camel_case_wire_keys() {
    let update = PermissionUpdate::AddRules {
        rules: vec![
            PermissionRuleValue {
                tool_name: "Edit".into(),
                rule_content: Some("*.py".into()),
            },
            PermissionRuleValue {
                tool_name: "Bash".into(),
                rule_content: None,
            },
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
        (
            PermissionUpdateDestination::ProjectSettings,
            "projectSettings",
        ),
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
        PermissionUpdate::SetMode {
            mode: PermissionMode::Plan,
            destination: None,
        },
        PermissionUpdate::AddDirectories {
            directories: vec!["/workspace".into()],
            destination: None,
        },
    ];
    let d = PermissionDecision::allow().with_updated_permissions(updates.clone());
    let got = d.updated_permissions();
    assert_eq!(got.len(), 2);
    assert_eq!(
        serde_json::to_value(got).unwrap(),
        serde_json::to_value(&updates).unwrap()
    );
}

#[test]
fn with_updated_permissions_is_noop_on_deny() {
    let d = PermissionDecision::deny("nope").with_updated_permissions(vec![
        PermissionUpdate::SetMode {
            mode: PermissionMode::Ask,
            destination: None,
        },
    ]);
    // Still a deny; permissions are not readable.
    assert!(d.updated_permissions().is_empty());
    assert_eq!(d.reason(), Some("nope"));
}
