//! File-index walker + filesystem watcher for the user's project
//! tree. Speaks the language of paths, `ignore::WalkBuilder` walks,
//! and `notify::Watcher` events; produces typed snapshots and change
//! batches without knowing anything about session routing.
//!
//! TUI consumers wrap these progress streams with their per-session
//! `SessionKey` + generation routing; the env layer is route-blind.
//!
//! Pattern mirrors `env::git_diff` (env owns subprocess + parsing,
//! workspace mediates, TUI consumes via channel).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

const SCAN_BATCH_SIZE: usize = 256;
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct FileCandidate {
    pub rel_path: String,
    pub rel_path_lower: String,
    pub basename_lower: String,
    pub depth: usize,
}

pub enum FileIndexChange {
    Upsert(FileCandidate),
    RemoveExact { rel_path: String },
    RemovePrefix { rel_prefix: String },
    ReplacePrefix { rel_prefix: String, entries: Vec<FileCandidate> },
}

/// Streaming-scan progress. Producers emit `Batch` repeatedly then
/// `Finished` once when the walk completes.
pub enum ScanProgress {
    Batch(Vec<FileCandidate>),
    Finished,
}

/// Watcher progress. `Changes` carries a delta batch; `Rebuild` is a
/// rescan signal (.gitignore changed or notify event we can't classify).
pub enum WatchProgress {
    Changes(Vec<FileIndexChange>),
    Rebuild,
}

/// Cancel guard for a backgrounded scan or watch. Drop to flip the
/// shared atomic that producers poll between batches / events.
pub struct CancelToken(Arc<AtomicBool>);

impl Drop for CancelToken {
    fn drop(&mut self) {
        self.0.store(true, AtomicOrdering::Relaxed);
    }
}

/// Spawn a streaming filesystem scan rooted at `root`. Returns a
/// receiver of [`ScanProgress`] batches (`Batch` until the walker
/// finishes, then one terminal `Finished`) and a cancel handle.
/// Dropping the cancel handle aborts the walker.
pub fn start_scan(root: PathBuf, respect_gitignore: bool) -> (Receiver<ScanProgress>, CancelToken) {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel);
    let (tx, rx) = mpsc::channel::<ScanProgress>();
    std::thread::spawn(move || {
        let mut batch = Vec::with_capacity(SCAN_BATCH_SIZE);
        let mut emit_candidate = |candidate| {
            batch.push(candidate);
            if batch.len() < SCAN_BATCH_SIZE {
                return true;
            }
            tx.send(ScanProgress::Batch(std::mem::take(&mut batch))).is_ok()
        };
        if !for_each_candidate(
            &root,
            &root,
            respect_gitignore,
            Some(&cancel_clone),
            &mut emit_candidate,
        ) {
            return;
        }
        if !batch.is_empty() && tx.send(ScanProgress::Batch(batch)).is_err() {
            return;
        }
        let _ = tx.send(ScanProgress::Finished);
    });
    (rx, CancelToken(cancel))
}

/// Spawn a recursive filesystem watch rooted at `root`. Returns a
/// receiver of [`WatchProgress`] events and a cancel handle.
/// Dropping the cancel handle stops the watcher.
pub fn start_watch(
    root: PathBuf,
    respect_gitignore: bool,
) -> (Receiver<WatchProgress>, CancelToken) {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel);
    let (tx, rx) = mpsc::channel::<WatchProgress>();
    std::thread::spawn(move || {
        let (watch_tx, watch_rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |result| {
            let _ = watch_tx.send(result);
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                tracing::warn!(target: "forge_agent::env::file_index", %err, "watcher setup failed");
                return;
            }
        };
        if let Err(err) =
            notify::Watcher::watch(&mut watcher, &root, notify::RecursiveMode::Recursive)
        {
            tracing::warn!(target: "forge_agent::env::file_index", %err, "watcher start failed");
            return;
        }

        while !cancel_clone.load(AtomicOrdering::Relaxed) {
            let event = match watch_rx.recv_timeout(WATCH_POLL_INTERVAL) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match event {
                Ok(event) => {
                    if let Some(progress) = classify_watch_event(&root, respect_gitignore, &event)
                        && tx.send(progress).is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    tracing::warn!(target: "forge_agent::env::file_index", %err, "watcher event failed");
                    if tx.send(WatchProgress::Rebuild).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (rx, CancelToken(cancel))
}

/// Synchronous walk that returns every candidate under `walk_root`.
/// Used by inline subtree refreshes triggered by watcher events.
pub fn collect_candidates(
    root: &Path,
    walk_root: &Path,
    respect_gitignore: bool,
) -> Vec<FileCandidate> {
    let mut candidates = Vec::new();
    for_each_candidate(root, walk_root, respect_gitignore, None, &mut |candidate| {
        candidates.push(candidate);
        true
    });
    candidates
}

/// Walk `walk_root` (rooted relative to `root`), invoking `emit` for
/// each `FileCandidate`. `emit` returning `false` cancels the walk.
/// Returns `true` when the walk completed naturally, `false` when
/// cancelled (either by `emit`'s return or the optional cancel flag).
fn for_each_candidate(
    root: &Path,
    walk_root: &Path,
    respect_gitignore: bool,
    cancel: Option<&Arc<AtomicBool>>,
    emit: &mut impl FnMut(FileCandidate) -> bool,
) -> bool {
    let mut builder = ignore::WalkBuilder::new(walk_root);
    builder
        .hidden(false)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore)
        .sort_by_file_path(std::cmp::Ord::cmp);

    for result in builder.build() {
        if cancel.is_some_and(|flag| flag.load(AtomicOrdering::Relaxed)) {
            return false;
        }
        let Ok(entry) = result else { continue };
        let Some(candidate) = candidate_from_entry(root, &entry) else { continue };
        if !emit(candidate) {
            return false;
        }
    }
    true
}

fn candidate_from_entry(root: &Path, entry: &ignore::DirEntry) -> Option<FileCandidate> {
    let ft = entry.file_type()?;
    let is_dir = ft.is_dir();
    let is_file = ft.is_file();
    if !is_dir && !is_file {
        return None;
    }
    let path = entry.path();
    let rel = path.strip_prefix(root).ok()?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str.is_empty() {
        return None;
    }
    let depth = rel_str.matches('/').count();
    let rel_path = if is_dir { format!("{rel_str}/") } else { rel_str };
    let rel_path_lower = rel_path.to_lowercase();
    let basename_lower = candidate_basename(&rel_path).to_lowercase();
    Some(FileCandidate { rel_path, rel_path_lower, basename_lower, depth })
}

pub fn candidate_basename(rel_path: &str) -> &str {
    let trimmed = rel_path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

fn normalize_relative_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    (!rel_str.is_empty()).then_some(rel_str)
}

fn normalized_prefix(root: &Path, path: &Path) -> Option<String> {
    normalize_relative_path(root, path).map(ensure_dir_suffix)
}

fn ensure_dir_suffix(mut rel_path: String) -> String {
    if !rel_path.ends_with('/') {
        rel_path.push('/');
    }
    rel_path
}

fn classify_watch_event(
    root: &Path,
    respect_gitignore: bool,
    event: &notify::Event,
) -> Option<WatchProgress> {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    if matches_ignore_semantics_change(root, &event.paths) {
        return Some(WatchProgress::Rebuild);
    }

    let changes = match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Any | RenameMode::Both)) => {
            collect_rename_changes(root, respect_gitignore, &event.paths)
        }
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder)
        | EventKind::Modify(
            ModifyKind::Any
            | ModifyKind::Data(_)
            | ModifyKind::Metadata(_)
            | ModifyKind::Name(RenameMode::To),
        ) => collect_create_or_modify_changes(root, respect_gitignore, &event.paths),
        EventKind::Modify(ModifyKind::Name(RenameMode::From))
        | EventKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Folder) => {
            collect_remove_changes(root, &event.paths)
        }
        EventKind::Other => return Some(WatchProgress::Rebuild),
        _ => Vec::new(),
    };

    (!changes.is_empty()).then_some(WatchProgress::Changes(changes))
}

fn matches_ignore_semantics_change(root: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| {
        let Some(rel) = normalize_relative_path(root, path) else {
            return false;
        };
        rel == ".gitignore"
            || rel == ".ignore"
            || rel.ends_with("/.gitignore")
            || rel.ends_with("/.ignore")
    }) || paths.iter().any(|path| {
        path.file_name().is_some_and(|name| name == "exclude")
            && path.parent().and_then(Path::file_name).is_some_and(|name| name == "info")
            && path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".git")
    })
}

fn collect_create_or_modify_changes(
    root: &Path,
    respect_gitignore: bool,
    paths: &[PathBuf],
) -> Vec<FileIndexChange> {
    let mut changes = Vec::new();
    for path in paths {
        if path.is_dir() {
            if let Some(change) = replace_subtree_change(root, path, respect_gitignore) {
                changes.push(change);
            }
        } else if path.is_file() {
            let mut entries = scan_subtree(root, path, respect_gitignore);
            if let Some(candidate) = entries.pop() {
                changes.push(FileIndexChange::Upsert(candidate));
            } else if let Some(rel_path) = normalize_relative_path(root, path) {
                changes.push(FileIndexChange::RemoveExact { rel_path });
            }
        }
    }
    changes
}

fn collect_remove_changes(root: &Path, paths: &[PathBuf]) -> Vec<FileIndexChange> {
    let mut changes = Vec::new();
    for path in paths {
        let Some(rel_path) = normalize_relative_path(root, path) else {
            continue;
        };
        changes.push(FileIndexChange::RemoveExact { rel_path: rel_path.clone() });
        changes.push(FileIndexChange::RemovePrefix { rel_prefix: ensure_dir_suffix(rel_path) });
    }
    changes
}

fn collect_rename_changes(
    root: &Path,
    respect_gitignore: bool,
    paths: &[PathBuf],
) -> Vec<FileIndexChange> {
    if paths.len() < 2 {
        // macOS FSEvents emits two separate RenameMode::Any events
        // (one per path) instead of a single paired event. If the
        // path no longer exists it is the "from" side of the rename
        // and should be treated as a remove.
        if paths.first().is_some_and(|p| !p.exists()) {
            return collect_remove_changes(root, paths);
        }
        return collect_parent_rescan_changes(root, respect_gitignore, paths);
    }
    collect_parent_rescan_changes(root, respect_gitignore, paths)
}

fn scan_subtree(root: &Path, path: &Path, respect_gitignore: bool) -> Vec<FileCandidate> {
    collect_candidates(root, path, respect_gitignore)
}

fn collect_parent_rescan_changes(
    root: &Path,
    respect_gitignore: bool,
    paths: &[PathBuf],
) -> Vec<FileIndexChange> {
    let mut changes = Vec::new();
    let mut seen_prefixes = BTreeSet::new();
    for path in paths {
        let Some(parent) = path.parent() else { continue };
        let Some(change) = replace_subtree_change(root, parent, respect_gitignore) else {
            continue;
        };
        let FileIndexChange::ReplacePrefix { rel_prefix, .. } = &change else {
            continue;
        };
        if seen_prefixes.insert(rel_prefix.clone()) {
            changes.push(change);
        }
    }
    changes
}

fn replace_subtree_change(
    root: &Path,
    path: &Path,
    respect_gitignore: bool,
) -> Option<FileIndexChange> {
    let rel_prefix = if path == root { String::new() } else { normalized_prefix(root, path)? };
    let entries = scan_subtree(root, path, respect_gitignore);
    Some(FileIndexChange::ReplacePrefix { rel_prefix, entries })
}
