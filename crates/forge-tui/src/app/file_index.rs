//! TUI-side state machine + routing for the file-index. The
//! filesystem walker and `notify::Watcher` themselves live in
//! `forge_agent::env::file_index` (lifted out so the TUI doesn't
//! shell out to OS-side I/O directly). This module:
//!
//! - Holds per-bucket [`FileIndexState`] (the `BTreeMap` of entries
//!   plus scan/watch handles).
//! - Spawns forwarding threads that consume the agent's progress
//!   channels and re-emit as `FileIndexEvent`s tagged with
//!   `SessionKey` + generation so the workspace-wide event pump
//!   routes them to the right bucket.
//! - Owns the reducer ([`apply_event`]) and the autocomplete
//!   ranking ([`visible_candidates`], [`rank_and_truncate_candidates`]).

use super::App;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, TryRecvError};

use forge_workspace::env::file_index as env;
pub use forge_workspace::env::file_index::{FileCandidate, FileIndexChange};

use super::MAX_CANDIDATES;

const EVENT_DRAIN_BUDGET: usize = 64;

#[derive(Default)]
pub struct FileIndexState {
    pub root: Option<PathBuf>,
    pub respect_gitignore: bool,
    pub generation: u64,
    pub entries: BTreeMap<String, FileCandidate>,
    pub scan_finished: bool,
    pub rebuild_pending: bool,
    scan_overrides: ScanOverrides,
    pub scan: Option<env::CancelToken>,
    pub watch: Option<env::CancelToken>,
}

pub enum FileIndexEvent {
    /// Each variant carries `key` so the workspace-wide
    /// `file_index_event_rx` pump can route to the right per-bucket
    /// `FileIndexState`. Without it, A's scanner output would land
    /// in B's index whenever B is active during A's scan.
    ScanBatch {
        key: forge_workspace::SessionKey,
        generation: u64,
        entries: Vec<FileCandidate>,
    },
    ScanFinished {
        key: forge_workspace::SessionKey,
        generation: u64,
    },
    FsBatch {
        key: forge_workspace::SessionKey,
        generation: u64,
        changes: Vec<FileIndexChange>,
    },
    RebuildRequested {
        key: forge_workspace::SessionKey,
        generation: u64,
    },
}

#[derive(Default)]
struct ScanOverrides {
    exact_paths: BTreeSet<String>,
    blocked_prefixes: Vec<String>,
}

pub fn reset(app: &mut App) {
    app.file_index_mut().generation = app.file_index_mut().generation.saturating_add(1);
    app.file_index_mut().root = None;
    app.file_index_mut().respect_gitignore = app.config.respect_gitignore_effective();
    app.file_index_mut().entries.clear();
    app.file_index_mut().scan_finished = false;
    app.file_index_mut().rebuild_pending = false;
    app.file_index_mut().scan_overrides = ScanOverrides::default();
    app.file_index_mut().scan = None;
    app.file_index_mut().watch = None;
}

pub fn restart(app: &mut App) {
    reset(app);
    let Some(key) = app.active_session_key.clone() else {
        return;
    };
    let root = PathBuf::from(app.cwd_raw());
    let generation = app.file_index_mut().generation;
    let respect_gitignore = app.config.respect_gitignore_effective();
    app.file_index_mut().root = Some(root.clone());
    app.file_index_mut().respect_gitignore = respect_gitignore;
    app.file_index_mut().scan_finished = false;
    app.file_index_mut().rebuild_pending = false;
    app.file_index_mut().scan_overrides = ScanOverrides::default();
    app.file_index_mut().scan = Some(spawn_scan(
        key.clone(),
        root.clone(),
        generation,
        respect_gitignore,
        app.file_index_event_tx.clone(),
    ));
    app.file_index_mut().watch = Some(spawn_watch(
        key,
        root,
        generation,
        respect_gitignore,
        app.file_index_event_tx.clone(),
    ));
}

pub fn ensure_started(app: &mut App) {
    let respect_gitignore = app.config.respect_gitignore_effective();
    let current_root = PathBuf::from(app.cwd_raw());
    let needs_restart = app.file_index_mut().root.as_ref() != Some(&current_root)
        || app.file_index_mut().respect_gitignore != respect_gitignore
        || (!app.file_index_mut().scan_finished && app.file_index_mut().scan.is_none());
    if needs_restart {
        restart(app);
    }
}

pub fn drain_events(app: &mut App) {
    let mut handled = 0;
    loop {
        if handled >= EVENT_DRAIN_BUDGET {
            break;
        }
        let event = match app.file_index_event_rx.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        };
        apply_event(app, event);
        handled += 1;
    }
}

pub fn visible_candidates(
    entries: &BTreeMap<String, FileCandidate>,
    query: &str,
) -> Vec<FileCandidate> {
    let query_lower = query.to_lowercase();
    let mut filtered: Vec<FileCandidate> = entries
        .values()
        .filter(|candidate| match_tier(candidate, &query_lower).is_some())
        .cloned()
        .collect();
    rank_and_truncate_candidates(&mut filtered, &query_lower);
    filtered
}

pub fn rank_and_truncate_candidates(candidates: &mut Vec<FileCandidate>, query_lower: &str) {
    candidates.sort_unstable_by(|a, b| {
        match_tier(a, query_lower)
            .cmp(&match_tier(b, query_lower))
            .then_with(|| a.depth.cmp(&b.depth))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    candidates.truncate(MAX_CANDIDATES);
}

fn match_tier(candidate: &FileCandidate, query_lower: &str) -> Option<u8> {
    if query_lower.is_empty() {
        return Some(0);
    }

    if candidate.basename_lower.starts_with(query_lower) {
        Some(0)
    } else if candidate.rel_path_lower.starts_with(query_lower) {
        Some(1)
    } else if candidate.basename_lower.contains(query_lower) {
        Some(2)
    } else if candidate.rel_path_lower.contains(query_lower) {
        Some(3)
    } else {
        None
    }
}

fn apply_event(app: &mut App, event: FileIndexEvent) {
    match event {
        FileIndexEvent::ScanBatch { key, generation, entries } => {
            let Some(slot) = app.sessions.get_mut(&key) else {
                return;
            };
            if generation != slot.file_index.generation {
                return;
            }
            for entry in entries {
                if slot.file_index.scan_overrides.blocks(&entry.rel_path) {
                    continue;
                }
                slot.file_index.entries.insert(entry.rel_path.clone(), entry);
            }
            refresh_after_mutation_if_active(app, &key);
        }
        FileIndexEvent::ScanFinished { key, generation } => {
            let Some(slot) = app.sessions.get_mut(&key) else {
                return;
            };
            if generation != slot.file_index.generation {
                return;
            }
            slot.file_index.scan_finished = true;
            slot.file_index.scan_overrides = ScanOverrides::default();
            slot.file_index.scan = None;
            refresh_after_mutation_if_active(app, &key);
        }
        FileIndexEvent::FsBatch { key, generation, changes } => {
            let Some(slot) = app.sessions.get_mut(&key) else {
                return;
            };
            if generation != slot.file_index.generation {
                return;
            }
            for change in changes {
                if !slot.file_index.scan_finished {
                    slot.file_index.scan_overrides.record_change(&change);
                }
                apply_change(&mut slot.file_index.entries, change);
            }
            refresh_after_mutation_if_active(app, &key);
        }
        FileIndexEvent::RebuildRequested { key, generation } => {
            // Only restart when the event targets the active session;
            // background buckets keep their stale generation until
            // they're switched-to (`switch_active_session` calls
            // `ensure_started`).
            let active_key = app.active_session_key.clone();
            if active_key.as_ref() != Some(&key) {
                return;
            }
            let active_generation = app.file_index().generation;
            if generation != active_generation {
                return;
            }
            restart(app);
            refresh_after_mutation(app);
        }
    }
}

/// Run `refresh_after_mutation` only when the just-mutated bucket
/// is the active one. Background-bucket mutations don't drive any
/// visible UI, so the @-mention refresh is a wasted hop.
fn refresh_after_mutation_if_active(app: &mut App, key: &forge_workspace::SessionKey) {
    if app.active_session_key.as_ref() == Some(key) {
        refresh_after_mutation(app);
    }
}

fn refresh_after_mutation(app: &mut App) {
    if app.mention().is_some() {
        super::mention::refresh_from_file_index(app);
    }
    app.needs_redraw = true;
}

fn apply_change(entries: &mut BTreeMap<String, FileCandidate>, change: FileIndexChange) {
    match change {
        FileIndexChange::Upsert(candidate) => {
            entries.insert(candidate.rel_path.clone(), candidate);
        }
        FileIndexChange::RemoveExact { rel_path } => {
            entries.remove(&rel_path);
        }
        FileIndexChange::RemovePrefix { rel_prefix } => {
            entries.retain(|path, _| !path.starts_with(&rel_prefix));
        }
        FileIndexChange::ReplacePrefix { rel_prefix, entries: next_entries } => {
            entries.retain(|path, _| !path.starts_with(&rel_prefix));
            for entry in next_entries {
                entries.insert(entry.rel_path.clone(), entry);
            }
        }
    }
}

/// Spawn the agent-side streaming scan and a forwarding thread that
/// wraps each [`env::ScanProgress`] with `key` + `generation` and
/// pushes it onto `event_tx`. Returns a handle whose Drop aborts
/// the agent walker.
fn spawn_scan(
    key: forge_workspace::SessionKey,
    root: PathBuf,
    generation: u64,
    respect_gitignore: bool,
    event_tx: Sender<FileIndexEvent>,
) -> env::CancelToken {
    let (rx, cancel) = env::start_scan(root, respect_gitignore);
    std::thread::spawn(move || {
        while let Ok(progress) = rx.recv() {
            let event = match progress {
                env::ScanProgress::Batch(entries) => {
                    FileIndexEvent::ScanBatch { key: key.clone(), generation, entries }
                }
                env::ScanProgress::Finished => {
                    FileIndexEvent::ScanFinished { key: key.clone(), generation }
                }
            };
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });
    cancel
}

/// Spawn the agent-side watcher and a forwarding thread that wraps
/// each [`env::WatchProgress`] with `key` + `generation`. Returns
/// a handle whose Drop stops the agent watcher.
fn spawn_watch(
    key: forge_workspace::SessionKey,
    root: PathBuf,
    generation: u64,
    respect_gitignore: bool,
    event_tx: Sender<FileIndexEvent>,
) -> env::CancelToken {
    let (rx, cancel) = env::start_watch(root, respect_gitignore);
    std::thread::spawn(move || {
        while let Ok(progress) = rx.recv() {
            let event = match progress {
                env::WatchProgress::Changes(changes) => {
                    FileIndexEvent::FsBatch { key: key.clone(), generation, changes }
                }
                env::WatchProgress::Rebuild => {
                    FileIndexEvent::RebuildRequested { key: key.clone(), generation }
                }
            };
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });
    cancel
}

impl ScanOverrides {
    fn record_change(&mut self, change: &FileIndexChange) {
        match change {
            FileIndexChange::Upsert(candidate) => {
                self.exact_paths.insert(candidate.rel_path.clone());
            }
            FileIndexChange::RemoveExact { rel_path } => {
                self.exact_paths.insert(rel_path.clone());
            }
            FileIndexChange::RemovePrefix { rel_prefix }
            | FileIndexChange::ReplacePrefix { rel_prefix, .. } => {
                self.blocked_prefixes.push(rel_prefix.clone());
            }
        }
    }

    fn blocks(&self, rel_path: &str) -> bool {
        self.exact_paths.contains(rel_path)
            || self.blocked_prefixes.iter().any(|prefix| rel_path.starts_with(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, mention};
    use std::time::{Duration, Instant, SystemTime};

    fn app_with_temp_files(files: &[&str]) -> (App, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let canonical = tmp.path().canonicalize().expect("canonicalize tempdir");
        for file in files {
            let path = canonical.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&path, "").expect("write file");
        }
        let mut app = App::test_default();
        app.set_cwd_raw(canonical.to_string_lossy().into_owned());
        (app, tmp)
    }

    fn wait_for(app: &mut App, timeout: Duration, mut predicate: impl FnMut(&App) -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            drain_events(app);
            if predicate(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drain_events(app);
        assert!(predicate(app), "condition not met before timeout");
    }

    fn candidate(rel_path: &str) -> FileCandidate {
        FileCandidate {
            rel_path: rel_path.to_owned(),
            rel_path_lower: rel_path.to_lowercase(),
            basename_lower: env::candidate_basename(rel_path).to_lowercase(),
            depth: rel_path.matches('/').count(),
            modified: SystemTime::UNIX_EPOCH,
            is_dir: rel_path.ends_with('/'),
        }
    }

    #[test]
    fn reopening_mention_reuses_existing_generation() {
        let (mut app, _tmp) = app_with_temp_files(&["src/main.rs"]);
        app.input_mut().set_text("@rs");
        let _ = app.input_mut().set_cursor(0, 3);

        mention::activate(&mut app);
        wait_for(&mut app, Duration::from_secs(2), |app| {
            app.file_index().scan_finished && !app.file_index().entries.is_empty()
        });
        let generation = app.file_index().generation;

        mention::deactivate(&mut app);
        app.input_mut().set_text("@src");
        let _ = app.input_mut().set_cursor(0, 4);
        mention::activate(&mut app);

        assert_eq!(app.file_index().generation, generation);
    }

    /// Regression for the `@`-mention wrong-project bug. Each bucket
    /// owns its own `FileIndexState`; a scan event targeting bucket
    /// B must NOT touch bucket A's index, even when A is the active
    /// bucket at delivery time.
    #[test]
    fn scan_event_routes_to_targeted_bucket_not_active_bucket() {
        let mut app = App::test_default();
        let key_a = forge_workspace::SessionKey::from_str_for_test("a");
        let key_b = forge_workspace::SessionKey::from_str_for_test("b");
        app.sessions
            .entry(key_a.clone())
            .or_insert_with(|| crate::app::session::UiSession::new(key_a.clone()));
        app.sessions
            .entry(key_b.clone())
            .or_insert_with(|| crate::app::session::UiSession::new(key_b.clone()));
        // Active bucket is A.
        app.active_session_key = Some(key_a.clone());
        // Bucket B's scanner emits a batch.
        let gen_b = app.sessions[&key_b].file_index.generation;
        apply_event(
            &mut app,
            FileIndexEvent::ScanBatch {
                key: key_b.clone(),
                generation: gen_b,
                entries: vec![candidate("only_in_b.rs")],
            },
        );
        // A's index is empty; B's index has the entry.
        assert!(app.sessions[&key_a].file_index.entries.is_empty());
        assert!(app.sessions[&key_b].file_index.entries.contains_key("only_in_b.rs"));
    }
}
