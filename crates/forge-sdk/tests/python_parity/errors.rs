//! Mirrors `tests/test_errors.py` from `claude-agent-sdk-python` v0.1.64.
//!
//! Python ships a class hierarchy — `ClaudeSDKError` base plus typed
//! subclasses with per-class constructors and Display formats. forge-sdk
//! exposes a single `Error` enum with comparable variants. These tests
//! check the behavioural intent of each Python test against the Rust
//! variant that serves the same role, and explicitly skip ports where
//! the Rust idiom has no 1-to-1 counterpart.

use std::error::Error as _;

use forge_sdk::Error;

/// Ported from `claude-agent-sdk-python` v0.1.64 `tests/test_errors.py::TestErrorTypes::test_base_error`.
///
/// Skipped — Python's `ClaudeSDKError("msg")` is a bare-message base
/// class. Rust's `Error` enum has no "untagged message" variant by
/// design; every surfaced failure lands in a typed variant. The closest
/// analogue is `Error::MessageParse { reason }`, covered by
/// [`message_parse_carries_reason`] below.
#[test]
#[ignore = "Python-only: no bare-message base variant in Rust Error enum"]
fn base_error() {}

/// Ported from `claude-agent-sdk-python` v0.1.64 `tests/test_errors.py::TestErrorTypes::test_cli_not_found_error`.
#[test]
fn cli_not_found_error() {
    let err = Error::CliNotFound {
        binary: "Claude Code not found".into(),
    };
    // Python asserts `isinstance(error, ClaudeSDKError)` — every Rust
    // `Error` variant satisfies `std::error::Error`, which is the
    // equivalent guarantee.
    let _: &dyn std::error::Error = &err;
    // Python asserts `"Claude Code not found" in str(error)`.
    assert!(err.to_string().contains("Claude Code not found"));
}

/// Ported from `claude-agent-sdk-python` v0.1.64 `tests/test_errors.py::TestErrorTypes::test_connection_error`.
#[test]
fn connection_error() {
    let err = Error::Connection {
        reason: "Failed to connect to CLI".into(),
    };
    let _: &dyn std::error::Error = &err;
    assert!(err.to_string().contains("Failed to connect to CLI"));
}

/// Ported from `claude-agent-sdk-python` v0.1.64 `tests/test_errors.py::TestErrorTypes::test_process_error`.
#[test]
fn process_error() {
    let err = Error::Process {
        exit_code: Some(1),
        stderr: "Command not found".into(),
    };

    // Python asserts the error exposes `exit_code` and `stderr`
    // accessors; mirror by destructuring the Rust variant.
    let Error::Process { exit_code, stderr } = &err else {
        panic!("expected Process variant");
    };
    assert_eq!(*exit_code, Some(1));
    assert_eq!(stderr, "Command not found");

    // Python asserts Display contains "exit code: 1" and the stderr
    // body. Rust's Display (`thiserror` expansion of `"...(exit code
    // {exit_code:?}): {stderr}"`) emits `Some(1)` rather than `1` —
    // so we assert the substrings that are genuinely informative:
    // the number `1` near the phrase "exit code", and the stderr body.
    let display = err.to_string();
    assert!(display.contains("exit code"), "got: {display}");
    assert!(display.contains('1'), "got: {display}");
    assert!(display.contains("Command not found"), "got: {display}");
}

/// Ported from `claude-agent-sdk-python` v0.1.64 `tests/test_errors.py::TestErrorTypes::test_json_decode_error`.
#[test]
fn json_decode_error() {
    let serde_err = serde_json::from_str::<serde_json::Value>("{invalid json}").unwrap_err();
    let err = Error::JsonDecode {
        line: 42,
        source: serde_err,
    };

    // Python asserts `error.line == "{invalid json}"` (i.e. raw source
    // line) — Rust's variant stores the 1-based LINE NUMBER instead, a
    // deliberate idiomatic difference. Assert both that the number is
    // preserved and that the underlying serde error is exposed as the
    // error source (Python's `original_error` analogue).
    let Error::JsonDecode { line, .. } = &err else {
        panic!("expected JsonDecode variant");
    };
    assert_eq!(*line, 42);
    assert!(err.source().is_some(), "JsonDecode must expose a source");

    // Python asserts `"Failed to decode JSON" in str(error)`. The Rust
    // Display phrasing is different; assert the informative bits: the
    // line number and the word "decode".
    let display = err.to_string();
    assert!(display.contains("decode"), "got: {display}");
    assert!(display.contains("42"), "got: {display}");
}

/// Additional Rust-side coverage for `Error::MessageParse`, the closest
/// sibling to Python's `MessageParseError`. Python has a matching unit
/// test but doesn't live in `test_errors.py`; this mirror stays close to
/// the `test_base_error` intent (carry a human-readable message verbatim).
#[test]
fn message_parse_carries_reason() {
    let err = Error::MessageParse {
        reason: "unknown message shape".into(),
    };
    let _: &dyn std::error::Error = &err;
    assert!(err.to_string().contains("unknown message shape"));
}
