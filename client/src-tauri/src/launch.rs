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
use std::sync::atomic::{AtomicI32, Ordering};
#[cfg(any(target_os = "android", test))]
use std::time::{Duration, SystemTime};

use forge_primitives::SessionSlot;
use tauri::Manager as _;

/// The activity launch this process last answered.
///
/// **The launch's seat belongs to one activity launch**: a later page load in
/// the same activity is a reload, but an activity the platform re-creates is a
/// new launch and asks again. Without the first half, a tap the app was too
/// late to report (a warm tap on Android) would pull the next reload to the
/// seat it could not open.
#[cfg(any(target_os = "android", test))]
static ANSWERED: AtomicI32 = AtomicI32::new(0);

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

    /// The seat of one notification, left for whichever reader comes next.
    fn peek(&self, id: i32) -> Option<SessionSlot> {
        let held = std::fs::read_to_string(self.file(id)).ok()?;
        match serde_json::from_str(&held) {
            Ok(seat) => Some(seat),
            Err(why) => {
                tauri_plugin_log::log::warn!("a remembered seat could not be read: {why}");
                None
            }
        }
    }

    /// The seat of one tapped notification, taken once: a page that asks
    /// again (a reload) is not pulled back to a seat it already opened.
    fn take(&self, id: i32) -> Option<SessionSlot> {
        let seat = self.peek(id);
        // **Taken whether or not it parses**: an unreadable entry can never
        // name a seat, and leaving it would keep it for the store's life.
        let _ = std::fs::remove_file(self.file(id));
        seat
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

/// The seat a notification tap launched the client on, taken once per launch.
///
/// Every launch that no notification opened answers `None`, which is every
/// launch on the desktop as well.
pub(crate) async fn take(app: &tauri::AppHandle) -> Option<SessionSlot> {
    let ask = tapped(app).await?;
    asked(ask, Seats::open(app).as_ref())
}

/// What one ask answers, and what it leaves behind.
///
/// **The launch is counted whether or not it names a tap**: a warm tap lands
/// on a launch whose first ask named nothing, and that ask must still arm the
/// latch, or the reload after the tap would take a seat the tap itself opened.
fn asked(ask: Ask, seats: Option<&Seats>) -> Option<SessionSlot> {
    answered(first_for(ask.launch), seats, ask.id)
}

/// What one ask answers, and what it leaves behind.
///
/// **The guard runs before the store is read**: a reload answers nothing and
/// must not take the seat either, or the activity launch that follows would
/// find it gone.
fn answered(first: bool, seats: Option<&Seats>, id: Option<i32>) -> Option<SessionSlot> {
    if !first {
        return None;
    }
    seats?.take(id?)
}

/// Whether this activity launch still owes the page its seat.
///
/// **Zero is the not-yet-a-launch sentinel**: no launch is numbered zero, so
/// the first launch's number differs from it and is answered.
#[cfg(any(target_os = "android", test))]
fn first_for(launch: i32) -> bool {
    ANSWERED.swap(launch, Ordering::SeqCst) != launch
}

#[cfg(not(any(target_os = "android", test)))]
fn first_for(_launch: i32) -> bool {
    true
}

/// The seat one notification names, left for the page's own ask.
///
/// **The same tap can arrive through two doors**: a warm tap is reported as
/// the activity's new intent while a cold start has the page ask for the same
/// id, and neither may consume the other's entry.
#[cfg(target_os = "android")]
pub(crate) fn seat(app: &tauri::AppHandle, id: i32) -> Option<SessionSlot> {
    Seats::open(app)?.peek(id)
}

/// What the activity says a launch names: the notification, if the intent is
/// a tap, and which activity launch is asking.
#[derive(serde::Deserialize)]
struct Ask {
    /// Missing when the intent is not a tap; the launch still counts.
    #[serde(default)]
    id: Option<i32>,
    /// The activity launch count, which tells a reload from a re-created
    /// activity.
    launch: i32,
}

/// The notification the activity was launched from, when a tap launched it.
#[cfg(target_os = "android")]
async fn tapped(app: &tauri::AppHandle) -> Option<Ask> {
    let reader = app.try_state::<std::sync::Arc<Reader>>()?;
    match reader.tapped().await {
        Ok(ask) => ask,
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
async fn tapped(_app: &tauri::AppHandle) -> Option<Ask> {
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
    async fn tapped(&self) -> Result<Option<Ask>, String> {
        self.0.run_mobile_plugin_async("tapped", ()).await.map_err(|why| why.to_string())
    }
}

/// A tap the Kotlin plugin reports while the app is already running.
#[cfg(target_os = "android")]
#[derive(serde::Deserialize)]
struct Tapped {
    id: i32,
}

/// The listener the Kotlin half reports a new intent through.
#[cfg(target_os = "android")]
#[derive(serde::Serialize)]
struct RegisterTap {
    event: &'static str,
    handler: tauri::ipc::Channel,
}

/// Register the Kotlin plugin that reads the launch intent, and the listener
/// its new intents arrive on.
#[cfg(target_os = "android")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("launchseat")
        .setup(|app, api| {
            let handle = api.register_android_plugin("dev.vedhavyas.forge", "LaunchSeatPlugin")?;
            let shown = app.clone();
            let channel = tauri::ipc::Channel::new(move |body| {
                if let tauri::ipc::InvokeResponseBody::Json(payload) = body
                    && let Ok(tapped) = serde_json::from_str::<Tapped>(&payload)
                {
                    crate::notify::warm_tap(&shown, tapped.id);
                }
                Ok(())
            });
            // **The reader is in place before the listener is asked for**: a
            // listener this platform refuses costs the warm tap, which is not
            // worth a cold start that cannot read its own launch either.
            app.manage(std::sync::Arc::new(Reader(handle.clone())));
            if let Err(why) = handle.run_mobile_plugin::<()>(
                "registerListener",
                RegisterTap { event: "tap", handler: channel },
            ) {
                tauri_plugin_log::log::warn!("the tap listener was not registered: {why}");
            }
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

    /// **An entry that cannot be read names nothing and goes**: a corrupt file
    /// must not sit in the store for its life.
    #[test]
    fn a_corrupt_entry_is_taken_and_names_nothing() {
        let (_dir, seats) = store();
        seats.record(7, &lead());
        std::fs::write(seats.file(7), b"not a seat").expect("this test's own file");
        assert_eq!(seats.take(7), None);
        assert!(!seats.file(7).exists(), "the unreadable entry was left behind");
    }

    /// **A seat is left for the next reader**: one tap can reach both doors -
    /// the warm report and the page's ask - so a read must not take it.
    #[test]
    fn a_read_leaves_the_seat_for_the_page() {
        let (_dir, seats) = store();
        seats.record(7, &lead());
        assert_eq!(seats.peek(7), Some(lead()), "the warm report reads the seat");
        assert_eq!(seats.peek(7), Some(lead()), "and leaves it where it was");
        assert_eq!(seats.take(7), Some(lead()), "so the page still gets it");
        assert_eq!(seats.peek(7), None, "and only the take removes it");
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

    /// **One activity launch answers once**: a page reload is not a launch,
    /// but an activity the platform re-creates is, and it asks again.
    #[test]
    fn one_activity_launch_answers_once() {
        assert!(first_for(41), "the launch's own ask is answered");
        assert!(!first_for(41), "a reload in the same activity is not");
        assert!(first_for(42), "a re-created activity is a new launch");
    }

    /// **A reload answers nothing and takes nothing**: the guard runs before
    /// the store is read, so the seat is still there for the activity launch
    /// that follows.
    #[test]
    fn a_reload_answers_nothing_and_leaves_the_seat() {
        let (_dir, seats) = store();
        seats.record(7, &lead());
        assert_eq!(answered(false, Some(&seats), Some(7)), None, "a reload is not the launch",);
        assert_eq!(seats.peek(7), Some(lead()), "and a reload does not take the seat");
        assert_eq!(
            answered(true, Some(&seats), Some(7)),
            Some(lead()),
            "the launch's own ask still answers it",
        );
    }

    /// **A launch that names no tap still arms the latch**: a warm tap lands
    /// on such a launch, and the reload after it must not take the seat the
    /// tap itself opened.
    #[test]
    fn a_launch_that_names_no_tap_still_arms_the_latch() {
        let (_dir, seats) = store();
        seats.record(7, &lead());
        // The launcher starts the app: the first ask names no tap.
        assert_eq!(asked(Ask { id: None, launch: 51 }, Some(&seats)), None);
        // A warm tap then opens the seat from the store without taking it ...
        assert_eq!(seats.peek(7), Some(lead()), "the warm report left the seat");
        // ... and a reload in the same activity asks again, now naming the tap.
        assert_eq!(
            asked(Ask { id: Some(7), launch: 51 }, Some(&seats)),
            None,
            "a reload after a warm tap must not open the seat",
        );
        assert_eq!(seats.peek(7), Some(lead()), "and must not take it either");
        // The next activity launch is the launch that answers it.
        assert_eq!(asked(Ask { id: Some(7), launch: 52 }, Some(&seats)), Some(lead()));
    }

    /// **Two notifications of one run never share an id**, or the second
    /// tap would open the first's seat.
    #[test]
    fn one_run_s_notifications_get_apart() {
        assert_ne!(next_id(), next_id());
    }
}
