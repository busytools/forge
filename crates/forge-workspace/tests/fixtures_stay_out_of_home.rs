//! Test-time guard: fixture `config_dir`s must never resolve into the
//! real home directory. A home-relative literal in a fixture gets
//! tilde-expanded, and when a test drives a spawn the CLI boots
//! against a real profile directory - writing `.claude.json` and
//! friends into `$HOME`, where it surfaces as a security-watchdog hit
//! rather than a test failure (#988 fixed the plain literals and the
//! escaped-string variants still recursed).
//!
//! Coverage: any .rs line containing both `config_dir` and `~` fails,
//! at any backslash-quoting depth. Phrase doc examples without
//! putting those two on one line.

use std::path::PathBuf;

#[test]
fn no_fixture_config_dir_resolves_into_the_home_directory() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates_dir = manifest_dir.parent().expect("crates/ sits beside the manifest");

    let mut files = Vec::new();
    let mut stack = vec![crates_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("walking the workspace crates/ tree");
        for entry in entries {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    assert!(
        files.len() > 100,
        "walked an implausibly small tree ({} files); the walk is broken",
        files.len()
    );

    let mut offenders = Vec::new();
    for file in &files {
        // The guard must name the pattern to search for it; it is the
        // one file exempt from its own scan.
        if file.file_name() == Some(std::ffi::OsStr::new("fixtures_stay_out_of_home.rs")) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(file) else {
            continue;
        };
        for (index, line) in source.lines().enumerate() {
            // Stripping backslashes collapses every quoting depth (plain
            // TOML strings, escaped format! strings, nested doc text)
            // into the same shape. Matching on the pair of substrings
            // rather than a full assignment form also catches
            // single-quoted TOML literals and no-whitespace `="~"`.
            let unescaped = line.replace('\\', "");
            if unescaped.contains("config_dir") && unescaped.contains('~') {
                offenders.push(format!("{}:{}: {}", file.display(), index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "fixture config_dirs must not resolve into $HOME - use a /tmp path so a \
         test that boots the CLI cannot write into a real profile; offenders:\n{}",
        offenders.join("\n")
    );
}
