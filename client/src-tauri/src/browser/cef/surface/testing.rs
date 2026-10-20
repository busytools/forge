//! The overlay view's ivars, checked where AppKit allows it.
//!
//! **NSView is main-thread-only**, so the check cannot be a `#[test]` body:
//! libtest runs those on a thread that is not the main one. `tests/overlay_ivars.rs`
//! is a harness-free target whose `main` calls this on the process main
//! thread, and the `testing` feature is what carries the entry point there.

use objc2::{DefinedClass as _, MainThreadMarker};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::OverlayView;
use super::input;

/// Build the view the way `attach` does, store and clear both slots, drop it.
///
/// **A real `AppHandle`, stored twice**, so the second assignment drops a live
/// handle: that `Some(AppHandle)` drop is the exact path the first browser
/// Open segfaulted on before the initializer called `set_ivars`.
pub fn the_overlay_view_ivars_hold(app: tauri::AppHandle) -> Result<(), String> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "the overlay check must run on the process main thread".to_owned())?;
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(8.0, 8.0));
    let view = OverlayView::new_with_frame(mtm.alloc::<OverlayView>(), frame);
    let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
    view.set_sender(input::Sender::new(tx));
    view.set_app(app.clone());
    view.set_app(app);
    // The Cmd+W read path, over the handle just stored.
    view.lower_surface();
    {
        let ivars = view.ivars();
        let mut sender = ivars.sender.lock().map_err(|why| format!("the sender slot: {why}"))?;
        if sender.is_none() {
            return Err("the sender was not stored in the initialized ivar".to_owned());
        }
        *sender = None;
        let app = ivars.app.lock().map_err(|why| format!("the app slot: {why}"))?;
        if app.is_none() {
            return Err("the app was not stored in the initialized ivar".to_owned());
        }
    }
    // The dealloc now drops both slots: the handle's drop glue runs here.
    drop(view);
    Ok(())
}
