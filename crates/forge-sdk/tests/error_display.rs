//! Smoke test: error enum variants all render with a useful Display message.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use forge_sdk::Error;

#[test]
fn cli_not_found_display() {
    let err = Error::CliNotFound {
        binary: "claude".into(),
    };
    let rendered = format!("{err}");
    assert!(
        rendered.contains("claude"),
        "expected binary in message, got: {rendered}"
    );
    assert!(
        rendered.to_lowercase().contains("not found"),
        "expected 'not found' in message, got: {rendered}"
    );
}

#[test]
fn process_error_display_includes_exit_code() {
    let err = Error::Process {
        exit_code: Some(17),
        stderr: "permission denied".into(),
    };
    let rendered = format!("{err}");
    assert!(
        rendered.contains("17"),
        "expected exit code 17, got: {rendered}"
    );
    assert!(
        rendered.contains("permission denied"),
        "expected stderr, got: {rendered}"
    );
}

#[test]
fn json_decode_error_display_includes_line_number() {
    let raw_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
    let err = Error::JsonDecode {
        line: 42,
        source: raw_err,
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("42"), "expected line 42, got: {rendered}");
}
