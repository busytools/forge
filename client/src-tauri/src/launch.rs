//! The seat a notification tap launched the client on.
//!
//! **The platform reads the launch intent before anything can hear it.** On
//! Android the notification plugin reports the tap the activity was started
//! with while the plugins initialize, before this app's click listener is
//! registered and before there is a page to receive it, so the report is
//! dropped and the app opens on its default route. The intent stays on the
//! activity and carries the id of the notification that was tapped, so the
//! seat is remembered here under that id when the notification is raised, and
//! the page that asks on mount is handed it.

use std::path::PathBuf;
#[cfg(any(target_os = "android", test))]
use std::time::{Duration, SystemTime};

use forge_primitives::SessionSlot;
use tauri::Manager as _;

/// How long a remembered seat outlives its notification.
///
/// Android drops the notifications themselves on a reboot, so this only
/// bounds what a notification the person never tapped leaves behind.
#[cfg(any(target_os = "android", test))]
const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The seats of raised notifications, by the id a launch intent carries.
///
/// **One file per id, not one table**: the writer is the notification thread
/// and the reader is the page's own command, and neither has to lock the
/// other out to find one notification.
struct Seats {
    dir: PathBuf,
}

impl Seats {
    /// The store under the app's data dir, or `None` when there is no such
    /// dir to write to.
    fn open(app: &tauri::AppHandle) -> Option<Self> {
        let dir = app.path().app_data_dir().ok()?.join("notification-seats");
        Some(Self { dir })
    }

    /// A store at `dir`, for a test that owns its own tree.
    #[cfg(test)]
    fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn file(&self, id: i32) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Remember the seat a raised notification opens, and drop what no
    /// notification can name any more.
    #[cfg(any(target_os = "android", test))]
    fn record(&self, id: i32, seat: &SessionSlot) {
        let held = match serde_json::to_string(seat) {
            Ok(held) => held,
            Err(why) => {
                tauri_plugin_log::log::warn!(
                    "the seat of {} was not remembered: {why}",
                    seat.display(),
                );
                return;
            }
        };
        let written =
            std::fs::create_dir_all(&self.dir).and_then(|()| std::fs::write(self.file(id), held));
        if let Err(why) = written {
            // **A read that fails here costs one cold tap its route**, so the
            // line names the seat that will open home instead.
            tauri_plugin_log::log::warn!(
                "the seat of {} was not remembered: {why}",
                seat.display(),
            );
            return;
        }
        self.prune(SystemTime::now());
    }

    /// The seat of one tapped notification, taken once: a page that asks
    /// again (a reload) is not pulled back to a seat it already opened.
    fn take(&self, id: i32) -> Option<SessionSlot> {
        let file = self.file(id);
        let held = std::fs::read_to_string(&file).ok()?;
        // **Taken whether or not it parses**: an unreadable entry can never
        // name a seat, and leaving it would keep it for the store's life.
        let _ = std::fs::remove_file(&file);
        match serde_json::from_str(&held) {
            Ok(seat) => Some(seat),
            Err(why) => {
                tauri_plugin_log::log::warn!("a remembered seat could not be read: {why}");
                None
            }
        }
    }

    #[cfg(any(target_os = "android", test))]
    fn prune(&self, now: SystemTime) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            if now.duration_since(modified).is_ok_and(|age| age > KEEP) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The seat a notification tap launched the client on, taken once.
///
/// Every launch that no notification opened answers `None`, which is every
/// launch on the desktop as well.
pub(crate) async fn take(app: &tauri::AppHandle) -> Option<SessionSlot> {
    let id = tapped(app).await?;
    Seats::open(app)?.take(id)
}

/// The id of the notification the activity was launched from, when a tap
/// launched it.
#[cfg(target_os = "android")]
async fn tapped(app: &tauri::AppHandle) -> Option<i32> {
    let reader = app.try_state::<std::sync::Arc<Reader>>()?;
    match reader.tapped().await {
        Ok(id) => id,
        Err(why) => {
            // A launch that cannot be read opens where it always did, so this
            // is a line rather than a failed boot.
            tauri_plugin_log::log::warn!(
                "the notification this launch came from could not be read: {why}",
            );
            None
        }
    }
}

#[cfg(not(target_os = "android"))]
async fn tapped(_app: &tauri::AppHandle) -> Option<i32> {
    None
}

/// The id a raised notification is remembered by.
///
/// **The plugin invents its own id**, which the shell never sees, so one is
/// chosen here: the clock seeds it and a counter keeps one run's notifications
/// apart, so an id a dead process used is not this run's.
#[cfg(any(target_os = "android", test))]
fn next_id() -> i32 {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicI32, Ordering};

    static SEED: OnceLock<i32> = OnceLock::new();
    static NEXT: AtomicI32 = AtomicI32::new(0);
    let seed = *SEED.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos() as i32)
    });
    seed.wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Remember a notification's seat before it is raised, and answer the id the
/// notification must carry for a tap on it to name the seat.
#[cfg(target_os = "android")]
pub(crate) fn remember(app: &tauri::AppHandle, seat: &SessionSlot) -> i32 {
    let id = next_id();
    match Seats::open(app) {
        Some(seats) => seats.record(id, seat),
        None => {
            tauri_plugin_log::log::warn!(
                "the seat of {} was not remembered: the app's data dir is not there",
                seat.display(),
            );
        }
    }
    id
}

/// The Kotlin handle: the notification the activity was launched from.
#[cfg(target_os = "android")]
pub struct Reader(tauri::plugin::PluginHandle<tauri::Wry>);

#[cfg(target_os = "android")]
impl Reader {
    async fn tapped(&self) -> Result<Option<i32>, String> {
        self.0.run_mobile_plugin_async("tapped", ()).await.map_err(|why| why.to_string())
    }
}

/// Register the Kotlin plugin that reads the launch intent.
#[cfg(target_os = "android")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("launchseat")
        .setup(|app, api| {
            let handle = api.register_android_plugin("dev.vedhavyas.forge", "LaunchSeatPlugin")?;
            app.manage(std::sync::Arc::new(Reader(handle)));
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn lead() -> SessionSlot {
        SessionSlot::lead("Busytools", "forge")
    }

    fn store() -> (tempfile::TempDir, Seats) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let seats = Seats::at(dir.path().to_path_buf());
        (dir, seats)
    }

    /// **A tap names the seat its notification was raised for**, and only
    /// once: a page that asks again must not be pulled back to it.
    #[test]
    fn a_tapped_notification_names_its_seat_once() {
        let (_dir, seats) = store();
        seats.record(7, &lead());
        assert_eq!(seats.take(7), Some(lead()));
        assert_eq!(seats.take(7), None, "the seat is taken once");
    }

    /// A launch no notification opened names nothing.
    #[test]
    fn an_unraised_notification_names_nothing() {
        let (_dir, seats) = store();
        assert_eq!(seats.take(9), None);
    }

    /// **One seat is not another's**: two outstanding notifications each keep
    /// their own, so which one was tapped decides which seat opens.
    #[test]
    fn each_notification_keeps_its_own_seat() {
        let (_dir, seats) = store();
        let worker = SessionSlot::worker("Busytools", "forge", "w1");
        seats.record(7, &lead());
        seats.record(8, &worker);
        assert_eq!(seats.take(8), Some(worker));
        assert_eq!(seats.take(7), Some(lead()));
    }

    /// **The litter is bounded by the notification's own life**: a seat no
    /// notification can name any more goes when the next one is raised, and
    /// the raise's own entry stays.
    #[test]
    fn a_raise_drops_seats_no_notification_can_name() {
        let (_dir, seats) = store();
        seats.record(1, &lead());
        let stale = seats.file(1);
        let old = SystemTime::now() - KEEP - Duration::from_secs(1);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("the entry this test wrote")
            .set_modified(old)
            .expect("an entry this test owns");

        seats.record(2, &lead());

        assert!(!stale.exists(), "an expired seat was kept");
        assert_eq!(seats.take(2), Some(lead()), "the raise dropped its own entry");
    }

    /// **Two notifications of one run never share an id**, or the second
    /// tap would open the first's seat.
    #[test]
    fn one_run_s_notifications_get_apart() {
        assert_ne!(next_id(), next_id());
    }
}
