//! A watch on exactly one file - `forge.toml` - so an edit reaches a
//! running forge without a restart.
//!
//! The watch is on the file, never its directory: nothing else in the
//! config dir can produce an event. An editor's atomic save writes a
//! temporary file and renames it over the path, and Syncthing applies an
//! incoming change the same way, so the watch is re-armed on the path
//! after every burst - a watch held on the replaced inode goes deaf.
//!
//! A symlink repointed at a different target produces no event, because the
//! backend watches the target's directory. Replace the file through the
//! link, or edit the path the config dir actually names.

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
#[must_use = "dropping it stops watching forge.toml"]
pub struct ConfigWatch {
    task: JoinHandle<()>,
}

impl Drop for ConfigWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Watch `path` and send one `()` per burst of content changes to it.
///
/// # Errors
///
/// There is no Tokio runtime to own the watch task, or the initial watcher
/// could not be created or armed. Whether that is fatal is the caller's
/// call: a running forge keeps serving and says it is not watching.
pub fn watch(path: PathBuf, changed: UnboundedSender<()>) -> std::io::Result<ConfigWatch> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        std::io::Error::other("config_watch::watch needs a Tokio runtime to own the watch")
    })?;
    let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = Some(arm(&path, &events)?);
    let task = runtime.spawn(async move {
        // The task's own sender keeps this channel open for the watch's
        // life, so a read never ends it; dropping the handle does.
        loop {
            let _ = incoming.recv().await;
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

/// A notify failure as the `io::Error` the re-arm classifies on.
/// `std::io::Error::other` would erase the one kind that matters here:
/// a path an atomic save has momentarily removed is expected, and
/// everything else is forge's own problem.
fn arm_io(error: notify::Error) -> std::io::Error {
    let kind = match &error.kind {
        notify::ErrorKind::PathNotFound => std::io::ErrorKind::NotFound,
        notify::ErrorKind::Io(io) => io.kind(),
        notify::ErrorKind::Generic(_)
        | notify::ErrorKind::WatchNotFound
        | notify::ErrorKind::InvalidConfig(_)
        | notify::ErrorKind::MaxFilesWatch => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error)
}

fn unwatchable(path: &Path, error: &std::io::Error) {
    tracing::warn!(
        target: "forge_agent::env::config_watch",
        event_name = "config_watch_rearm_failed",
        path = %path.display(),
        error = %error,
        "forge.toml could not be watched after an edit; the last good \
         config stays live and the watch keeps trying",
    );
}

/// Whether one notify event means the watched file may have changed. A
/// rescan says events were dropped and anything may have moved, and it
/// carries no paths, so it is decided before the path test.
fn event_matches(event: &notify::Event, target: &WatchedPath) -> bool {
    if event.need_rescan() {
        return true;
    }
    super::is_content_change(event.kind)
        && event.paths.iter().any(|event_path| target.matches(event_path))
}

fn arm(path: &Path, events: &UnboundedSender<()>) -> std::io::Result<RecommendedWatcher> {
    let target = WatchedPath::new(path);
    let events = events.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else { return };
        if event_matches(&event, &target) {
            let _ = events.send(());
        }
    })
    .map_err(arm_io)?;
    watcher.watch(path, RecursiveMode::NonRecursive).map_err(arm_io)?;
    Ok(watcher)
}

/// Re-arm for up to [`REARM_GRACE`], covering the gap an atomic save
/// leaves. `None` means the path is still missing; the caller sends its
/// notification anyway and [`slow_rearm`] keeps trying.
async fn rearm_fast(path: &Path, events: &UnboundedSender<()>) -> Option<RecommendedWatcher> {
    let deadline = tokio::time::Instant::now() + REARM_GRACE;
    let mut warned = false;
    loop {
        match arm(path, events) {
            Ok(watcher) => return Some(watcher),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound && !warned {
                    unwatchable(path, &error);
                    warned = true;
                }
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
    let mut warned = false;
    loop {
        tokio::time::sleep(REARM_SLOW).await;
        if changed.is_closed() {
            return None;
        }
        match arm(path, events) {
            Ok(watcher) => {
                let _ = changed.send(());
                return Some(watcher);
            }
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound && !warned {
                    unwatchable(path, &error);
                    warned = true;
                }
            }
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

    async fn fixture() -> (tempfile::TempDir, std::path::PathBuf, UnboundedReceiver<()>, ConfigWatch)
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forge.toml");
        std::fs::write(&path, "initial = true\n").expect("write config");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = watch(path.clone(), tx).expect("watch starts");
        // The write that created the file can mint its event just after the
        // stream starts. Rewrite once and drain that burst, so each test
        // begins from a quiet watch rather than racing the fixture's own
        // write.
        std::fs::write(&path, "initial = true\n").expect("settle write");
        let _ = tokio::time::timeout(WAIT, rx.recv()).await;
        (dir, path, rx, watch)
    }

    async fn next(rx: &mut UnboundedReceiver<()>) -> bool {
        tokio::time::timeout(WAIT, rx.recv()).await.is_ok()
    }

    #[tokio::test]
    async fn a_plain_write_notifies_once() {
        let (_dir, path, mut rx, _watch) = fixture().await;
        std::fs::write(&path, "changed = 1\n").expect("write");
        assert!(next(&mut rx).await, "a write to the watched file notifies");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "one write is one notification, not a second after the burst",
        );
    }

    /// The reload's own read must not feed the watch. inotify reports a read
    /// of a single watched file as an access event; accepting it would make
    /// every reload trigger the next one.
    #[tokio::test]
    async fn a_read_does_not_notify() {
        let (_dir, path, mut rx, _watch) = fixture().await;
        std::fs::write(&path, "before the read = 1\n").expect("write");
        assert!(next(&mut rx).await, "the watch is live before the read");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "one write is one notification",
        );
        std::fs::read_to_string(&path).expect("read the watched file");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "reading the watched file is not a content change",
        );
    }

    /// A rescan says events were dropped and names no path, so it must
    /// reach the caller anyway; a read names the file and still must not.
    #[test]
    fn a_rescan_matches_and_a_read_does_not() {
        use notify::event::{AccessKind, EventKind, Flag};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forge.toml");
        let target = super::WatchedPath::new(&path);

        let rescan = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(super::event_matches(&rescan, &target), "a rescan triggers a reload");

        let read = notify::Event::new(EventKind::Access(AccessKind::Any)).add_path(path);
        assert!(!super::event_matches(&read, &target), "a read is not a content change");
    }

    /// **The re-arm is what the Linux run proves.** macOS FSEvents survives a
    /// rename-over without it, so a green run there says nothing about the
    /// re-arm; the second write is the assertion that matters on Linux.
    #[tokio::test]
    async fn an_atomic_save_notifies_and_the_watch_survives_the_rename() {
        let (_dir, path, mut rx, _watch) = fixture().await;
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
        let (_dir, path, mut rx, _watch) = fixture().await;
        std::fs::write(&path, "first = 1\n").expect("first write");
        std::fs::write(&path, "second = 1\n").expect("second write");
        assert!(next(&mut rx).await, "the burst notifies");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "both writes land in one burst, so the caller reloads once",
        );
    }

    /// The negative control carries its own positive half: a watched write
    /// notifies exactly once before the sibling write proves silence, so a
    /// dead watcher cannot pass the test.
    #[tokio::test]
    async fn a_sibling_file_notifies_nothing() {
        let (_dir, path, mut rx, _watch) = fixture().await;
        std::fs::write(&path, "changed = 1\n").expect("write the watched file");
        assert!(next(&mut rx).await, "the watched file still notifies");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "one watched write is one notification",
        );

        std::fs::write(path.with_file_name("other.toml"), "sibling = 1\n").expect("sibling");
        assert!(
            tokio::time::timeout(QUIET, rx.recv()).await.is_err(),
            "only the watched path produces an event",
        );
    }

    /// A file that stays missing past the grace window is still reported, and
    /// the slow re-arm deliberately keeps trying so the file's return applies.
    #[tokio::test]
    async fn a_deleted_file_still_notifies_and_its_return_applies() {
        let (_dir, path, mut rx, _watch) = fixture().await;
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
