//! Tests for the `skills` / `allowed_tools` / `setting_sources` options.
//!
//! These verify the options plumbing; CLI-arg emission is tested indirectly
//! via real-claude smoke tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::OptionsBuilder;

#[test]
fn skills_default_empty() {
    let opts = OptionsBuilder::new().build();
    assert!(opts.skills.is_empty());
    assert!(opts.allowed_tools.is_empty());
    assert!(opts.setting_sources.is_none());
    assert!(!opts.exclude_dynamic_sections);
}

#[test]
fn skills_with_all_marker() {
    let opts = OptionsBuilder::new().skills(["all"]).build();
    assert_eq!(opts.skills, vec!["all".to_string()]);
}

#[test]
fn skills_with_concrete_names() {
    let opts = OptionsBuilder::new()
        .skills(["create-story", "another-skill"])
        .build();
    assert_eq!(opts.skills.len(), 2);
}

#[test]
fn explicit_setting_sources_override_default() {
    let opts = OptionsBuilder::new()
        .skills(["create-story"])
        .setting_sources(["local"])
        .build();
    assert_eq!(opts.setting_sources, Some(vec!["local".to_string()]));
}

#[test]
fn allowed_tools_round_trip() {
    let opts = OptionsBuilder::new()
        .allowed_tools(["Read", "Grep"])
        .build();
    assert_eq!(opts.allowed_tools, vec!["Read".to_string(), "Grep".into()]);
}

#[test]
fn exclude_dynamic_sections_toggles() {
    let opts = OptionsBuilder::new().exclude_dynamic_sections(true).build();
    assert!(opts.exclude_dynamic_sections);
}
