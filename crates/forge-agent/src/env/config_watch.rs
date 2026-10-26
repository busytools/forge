//! A watch on exactly one file - `forge.toml` - so an edit reaches a
//! running forge without a restart.
//!
//! The watch is on the file, never its directory: nothing else in the
//! config dir can produce an event. An editor's atomic save writes a
//! temporary file and renames it over the path, and Syncthing applies an
//! incoming change the same way, so the watch is re-armed on the path
//! after every burst - a watch held on the replaced inode goes deaf.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// Coalescing window, measured from a burst's first event. Later events
/// inside it join the burst and do not extend it.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Re-arm retry while an atomic save has the path momentarily missing.
const REARM_RETRY: Duration = Duration::from_millis(100);
/// How long the fast retries run before the missing path is reported.
const REARM_GRACE: Duration = Duration::from_secs(2);
/// Re-arm cadence after that, so a file that reappears later is still
/// watched and its contents apply.
const REARM_SLOW: Duration = Duration::from_secs(1);

/// A live watch. Dropping it stops the task and with it the watcher.
pub struct ConfigWatch {
    task: JoinHandle<()>,
}

impl Drop for ConfigWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Watch `path` and send one `()` per burst of changes to it.
///
/// # Errors
///
/// The initial watcher could not be created or armed, which is the
/// caller's boot failure to report.
pub fn watch(path: PathBuf, changed: UnboundedSender<()>) -> std::io::Result<ConfigWatch> {
    let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = Some(arm(&path, &events)?);
    let task = tokio::spawn(async move {
        while incoming.recv().await.is_some() {
            let deadline = tokio::time::Instant::now() + DEBOUNCE;
            loop {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => break,
                    Some(()) = incoming.recv() => {}
                }
            }
            while incoming.try_recv().is_ok() {}

            drop(watcher.take());
            watcher = rearm_fast(&path, &events).await;
            let rearmed = watcher.is_some();
            if changed.send(()).is_err() {
                break;
            }
            if !rearmed {
                watcher = slow_rearm(&path, &events, &changed).await;
                if watcher.is_none() {
                    break;
                }
            }
        }
        drop(watcher);
    });
    Ok(ConfigWatch { task })
}

/// Both spellings of the watched path: FSEvents reports real paths, so a
/// config dir reached through a symlink (`/var` on macOS) needs the
/// canonical one to match.
struct WatchedPath {
    raw: PathBuf,
    canonical: Option<PathBuf>,
}

impl WatchedPath {
    fn new(path: &Path) -> Self {
        Self { raw: path.to_path_buf(), canonical: path.canonicalize().ok() }
    }

    fn matches(&self, event: &Path) -> bool {
        event == self.raw || self.canonical.as_deref() == Some(event)
    }
}

fn arm(path: &Path, events: &UnboundedSender<()>) -> std::io::Result<RecommendedWatcher> {
    let target = WatchedPath::new(path);
    let events = events.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else { return };
        if event.paths.iter().any(|event_path| target.matches(event_path)) {
            let _ = events.send(());
        }
    })
    .map_err(std::io::Error::other)?;
    watcher.watch(path, RecursiveMode::NonRecursive).map_err(std::io::Error::other)?;
    Ok(watcher)
}

/// Re-arm for up to [`REARM_GRACE`], covering the gap an atomic save
/// leaves. `None` means the path is still missing; the caller sends its
/// notification anyway and [`slow_rearm`] keeps trying.
async fn rearm_fast(path: &Path, events: &UnboundedSender<()>) -> Option<RecommendedWatcher> {
    let deadline = tokio::time::Instant::now() + REARM_GRACE;
    loop {
        match arm(path, events) {
            Ok(watcher) => return Some(watcher),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    tracing::debug!(
                        target: "forge_agent::env::config_watch",
                        event_name = "config_watch_rearm_waiting",
                        path = %path.display(),
                        error = %error,
                        "the config file is not back yet; the last good config stays live",
                    );
                    return None;
                }
                tokio::time::sleep(REARM_RETRY).await;
            }
        }
    }
}

/// The path was missing past the grace window: keep a slow watch attempt
/// running so a file that comes back later is seen and applies.
async fn slow_rearm(
    path: &Path,
    events: &UnboundedSender<()>,
    changed: &UnboundedSender<()>,
) -> Option<RecommendedWatcher> {
    loop {
        tokio::time::sleep(REARM_SLOW).await;
        if changed.is_closed() {
            return None;
        }
        if let Ok(watcher) = arm(path, events) {
            let _ = changed.send(());
            return Some(watcher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigWatch, watch};
    use std::time::Duration;
    use tokio::sync::mpsc::UnboundedReceiver;

    const WAIT: Duration = Duration::from_secs(5);
    const QUIET: Duration = Duration::from_millis(800);

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, UnboundedReceiver<()>, ConfigWatch) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forge.toml");
        std::fs::write(&path, "initial = true\n").expect("write config");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = watch(path.clone(), tx).expect("watch starts");
        (dir, path, rx, watch)
    }

    async fn next(rx: &mut UnboundedReceiver<()>) -> bool {
        tokio::time::timeout(WAIT, rx.recv()).await.is_ok()
    }

    #[tokio::test]
    async fn a_plain_write_notifies_once() {
        let (_dir, path, mut rx, _watch) = fixture();
        std::fs::write(&path, "changed = 1\n").expect("write");
        assert!(next(&mut rx).await, "a write to the watched file notifies");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "one write is one notification, not a second after the burst",
        );
    }

    #[tokio::test]
    async fn an_atomic_save_notifies_and_the_watch_survives_the_rename() {
        let (_dir, path, mut rx, _watch) = fixture();
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, "replaced = 1\n").expect("write temp");
        std::fs::rename(&tmp, &path).expect("rename over the config");
        assert!(next(&mut rx).await, "a temp-file-plus-rename notifies");

        std::fs::write(&path, "replaced = 2\n").expect("write after the rename");
        assert!(
            next(&mut rx).await,
            "the watch was re-armed on the path, so the next save still notifies",
        );
    }

    #[tokio::test]
    async fn two_rapid_writes_coalesce_into_one() {
        let (_dir, path, mut rx, _watch) = fixture();
        std::fs::write(&path, "first = 1\n").expect("first write");
        std::fs::write(&path, "second = 1\n").expect("second write");
        assert!(next(&mut rx).await, "the burst notifies");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "both writes land in one burst, so the caller reloads once",
        );
    }

    #[tokio::test]
    async fn a_sibling_file_notifies_nothing() {
        let (_dir, path, mut rx, _watch) = fixture();
        std::fs::write(path.with_file_name("other.toml"), "sibling = 1\n").expect("sibling");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "only the watched path produces an event",
        );
    }

    #[tokio::test]
    async fn a_deleted_file_still_notifies_and_its_return_applies() {
        let (_dir, path, mut rx, _watch) = fixture();
        std::fs::remove_file(&path).expect("remove");
        assert!(
            next(&mut rx).await,
            "a file gone past the grace window still reaches the caller, so the missing \
             path is reported",
        );

        std::fs::write(&path, "back = 1\n").expect("recreate");
        assert!(
            next(&mut rx).await,
            "a config that comes back is seen again, so its contents apply",
        );
    }
}
