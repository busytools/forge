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
/// Upper bound on events coalesced into one pass, so a sustained
/// writer cannot starve the cancel check at the top of the loop.
const WATCH_BATCH_CAP: usize = 1024;

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

        let mut filter = WatchFilter::new(&root, respect_gitignore);
        while !cancel_clone.load(AtomicOrdering::Relaxed) {
            let first = match watch_rx.recv_timeout(WATCH_POLL_INTERVAL) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            // Drain whatever else is already queued. A build emits
            // events in bursts and each one used to be an independent
            // rescan; taking the burst in one pass lets the dedupe in
            // `collect_parent_rescan_changes` collapse them.
            let mut batch = vec![first];
            while let Ok(event) = watch_rx.try_recv() {
                batch.push(event);
                if batch.len() >= WATCH_BATCH_CAP {
                    break;
                }
            }

            let mut changes = Vec::new();
            let mut rebuild = false;
            for event in batch {
                match event {
                    Ok(event) => {
                        match classify_watch_event(&root, respect_gitignore, &filter, &event) {
                            Some(WatchProgress::Rebuild) => rebuild = true,
                            Some(WatchProgress::Changes(mut batch_changes)) => {
                                changes.append(&mut batch_changes);
                            }
                            None => {}
                        }
                    }
                    Err(err) => {
                        tracing::warn!(target: "forge_agent::env::file_index", %err, "watcher event failed");
                        rebuild = true;
                    }
                }
            }

            if rebuild {
                // The ignore rules themselves may be what changed, so
                // the matcher has to come back with them.
                filter = WatchFilter::new(&root, respect_gitignore);
                if tx.send(WatchProgress::Rebuild).is_err() {
                    break;
                }
                continue;
            }
            if !changes.is_empty() && tx.send(WatchProgress::Changes(changes)).is_err() {
                break;
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

/// Decides whether a watcher event path is worth acting on at all.
///
/// The watcher registers the whole root recursively and `notify` has
/// no concept of ignore rules, so events under `target/` arrive like
/// any other. Handing one to the walk does NOT filter it either: the
/// `ignore` crate applies its rules to what it finds under a walk root
/// and never to the root itself, so pointing a walk at a changed
/// directory inside an ignored tree walks the whole tree. Matching
/// here, against a matcher built once per root, is what keeps a cargo
/// build in a watched project from costing a full rescan per event.
pub(crate) struct WatchFilter {
    matcher: Option<ignore::gitignore::Gitignore>,
    /// The matcher is rooted here and every path is rebased onto it
    /// before matching. Two reasons, and the second one bites hard:
    /// the watcher reports canonical paths, so on macOS it says
    /// `/private/var/...` where the configured root says `/var/...`;
    /// and `matched_path_or_any_parents` PANICS on a path that is not
    /// under its root, which would take the watcher thread down on
    /// the first event rather than merely failing to filter.
    canonical_root: PathBuf,
}

impl WatchFilter {
    pub(crate) fn new(root: &Path, respect_gitignore: bool) -> Self {
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if !respect_gitignore {
            return Self { matcher: None, canonical_root };
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&canonical_root);
        for candidate in [".gitignore", ".ignore"] {
            builder.add(root.join(candidate));
        }
        builder.add(root.join(".git").join("info").join("exclude"));
        Self { matcher: builder.build().ok(), canonical_root }
    }

    fn is_ignored(&self, root: &Path, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(root).or_else(|_| path.strip_prefix(&self.canonical_root))
        else {
            return false;
        };
        if rel.components().next().is_some_and(|first| first.as_os_str() == ".git") {
            return true;
        }
        let Some(matcher) = self.matcher.as_ref() else { return false };
        // `matched_path_or_any_parents` is the variant that works
        // without having walked down to the path; the plain `matched`
        // would miss `target/debug/x` against a `/target` rule. It is
        // also the one that panics off-root, hence the rebase.
        let rebased = self.canonical_root.join(rel);
        matcher.matched_path_or_any_parents(&rebased, rebased.is_dir()).is_ignore()
    }
}

/// Whether an event kind can actually change what the ignore rules
/// mean. A read cannot, and it matters: building the matcher reads
/// `.gitignore` from inside the watched tree, and on inotify a read is
/// itself a watchable event. Treating one as a rebuild trigger makes
/// the rebuild feed itself.
fn is_content_change(kind: notify::EventKind) -> bool {
    use notify::event::{EventKind, ModifyKind};
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Name(_))
    )
}

fn classify_watch_event(
    root: &Path,
    respect_gitignore: bool,
    filter: &WatchFilter,
    event: &notify::Event,
) -> Option<WatchProgress> {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    if is_content_change(event.kind) && matches_ignore_semantics_change(root, &event.paths) {
        return Some(WatchProgress::Rebuild);
    }

    let paths: Vec<PathBuf> =
        event.paths.iter().filter(|path| !filter.is_ignored(root, path)).cloned().collect();
    if paths.is_empty() {
        return None;
    }

    let changes = match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Any | RenameMode::Both)) => {
            collect_rename_changes(root, respect_gitignore, &paths)
        }
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder)
        | EventKind::Modify(
            ModifyKind::Any
            | ModifyKind::Data(_)
            | ModifyKind::Metadata(_)
            | ModifyKind::Name(RenameMode::To),
        ) => collect_create_or_modify_changes(root, respect_gitignore, &paths),
        EventKind::Modify(ModifyKind::Name(RenameMode::From))
        | EventKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Folder) => {
            collect_remove_changes(root, &paths)
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, EventKind};

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    fn create_event(path: &Path) -> notify::Event {
        notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![path.to_path_buf()],
            attrs: notify::event::EventAttributes::new(),
        }
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "/target\n/node_modules\n").unwrap();
        touch(&root.join("src/main.rs"));
        touch(&root.join("target/debug/liba.rlib"));
        dir
    }

    #[test]
    fn an_event_inside_a_gitignored_directory_is_dropped() {
        // #523: the watcher registers the whole root recursively and
        // has no ignore filter, so a cargo build under a gitignored
        // `target/` used to reach `collect_candidates`. That builds a
        // fresh WalkBuilder aimed at the changed path, and the ignore
        // crate never filters a walk's own ROOT - so the whole tree
        // got walked and sorted for an event we do not care about.
        let dir = fixture();
        let root = dir.path();
        let filter = WatchFilter::new(root, true);
        let event = create_event(&root.join("target/debug/liba.rlib"));
        assert!(classify_watch_event(root, true, &filter, &event).is_none());
    }

    #[test]
    fn an_event_on_a_tracked_file_still_classifies() {
        let dir = fixture();
        let root = dir.path();
        let filter = WatchFilter::new(root, true);
        let event = create_event(&root.join("src/main.rs"));
        assert!(matches!(
            classify_watch_event(root, true, &filter, &event),
            Some(WatchProgress::Changes(_))
        ));
    }

    #[test]
    fn ignoring_is_off_when_gitignore_is_not_respected() {
        // `respect_gitignore = false` must keep seeing everything;
        // the filter is not a second, independent policy.
        let dir = fixture();
        let root = dir.path();
        let filter = WatchFilter::new(root, false);
        let event = create_event(&root.join("target/debug/liba.rlib"));
        assert!(classify_watch_event(root, false, &filter, &event).is_some());
    }

    #[test]
    fn a_gitignore_edit_still_forces_a_rebuild() {
        // The rebuild signal must survive the filter, otherwise the
        // matcher can never be refreshed.
        let dir = fixture();
        let root = dir.path();
        let filter = WatchFilter::new(root, true);
        let event = create_event(&root.join(".gitignore"));
        assert!(matches!(
            classify_watch_event(root, true, &filter, &event),
            Some(WatchProgress::Rebuild)
        ));
    }

    #[test]
    fn reading_the_gitignore_does_not_trigger_a_rebuild() {
        // #523: building the matcher reads `.gitignore` from inside
        // the watched tree, and on inotify a read is a watchable
        // event. When any event kind on that file forced a rebuild,
        // and every rebuild rebuilt the matcher, the rebuild fed
        // itself. Only a content change may trigger one.
        let dir = fixture();
        let root = dir.path();
        let filter = WatchFilter::new(root, true);
        let read = notify::Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![root.join(".gitignore")],
            attrs: notify::event::EventAttributes::new(),
        };
        assert!(classify_watch_event(root, true, &filter, &read).is_none());
    }

    #[test]
    fn the_filter_still_matches_when_the_root_is_a_symlink() {
        // macOS hands out `/var/...` while the watcher reports
        // `/private/var/...`. Without the canonical fallback the
        // strip_prefix fails for every event and the whole filter
        // silently passes everything through - which is exactly how
        // this fix could look like it worked while doing nothing.
        let dir = fixture();
        let root = dir.path();
        let canonical = root.canonicalize().unwrap();
        let filter = WatchFilter::new(root, true);
        let event = create_event(&canonical.join("target/debug/liba.rlib"));
        assert!(classify_watch_event(root, true, &filter, &event).is_none());
    }

    #[test]
    fn the_git_directory_is_dropped_without_a_gitignore_rule() {
        // `.git/` is never listed in .gitignore but churns constantly
        // during any git operation.
        let dir = fixture();
        let root = dir.path();
        let filter = WatchFilter::new(root, true);
        let event = create_event(&root.join(".git/index.lock"));
        assert!(classify_watch_event(root, true, &filter, &event).is_none());
    }
}
