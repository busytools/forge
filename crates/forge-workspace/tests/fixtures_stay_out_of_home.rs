//! Test-time guard: fixture `config_dir`s must never resolve into the
//! real home directory. A home-relative literal in a fixture gets
//! tilde-expanded, and when a test drives a spawn the CLI boots
//! against a real profile directory - writing `.claude.json` and
//! friends into `$HOME`, where it surfaces as a security-watchdog hit
//! rather than a test failure (#988 fixed the plain literals and the
//! escaped-string variants still recursed). This guard rejects every
//! quoting shape, so re-introducing the pattern fails CI here instead
//! of writing to a live profile.

use std::path::{Path, PathBuf};

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => panic!("walking the workspace crates/ tree: {err}"),
    };
    for entry in entries {
        let Ok(entry) = entry else { panic!("unreadable entry under {}", dir.display()) };
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_fixture_config_dir_resolves_into_the_home_directory() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates_dir = manifest_dir.parent().expect("crates/ sits beside the manifest").to_owned();

    let mut files = Vec::new();
    collect_rs_files(&crates_dir, &mut files);
    assert!(
        files.len() > 100,
        "walked an implausibly small tree ({} files); the walk is broken",
        files.len()
    );

    let quote = '"';
    let needle = format!("config_dir = {quote}~");
    let mut offenders = Vec::new();
    for file in &files {
        let Ok(source) = std::fs::read_to_string(file) else {
            continue;
        };
        for (index, line) in source.lines().enumerate() {
            // Stripping backslashes collapses every quoting depth (plain
            // TOML strings, escaped format! strings, nested doc text)
            // into the same shape, so one check covers all of them.
            let unescaped = line.replace('\\', "");
            if unescaped.contains(&needle) {
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
