//! The overlay view's ivars, checked where AppKit allows it.
//!
//! **NSView is main-thread-only**, so the check cannot be a `#[test]` body:
//! libtest runs those on a thread that is not the main one.
//! `tests/overlay_ivars.rs` is a harness-free target whose `main` calls this
//! on the process main thread, and the `testing` feature is what carries the
//! entry point there.

use objc2::{DefinedClass as _, MainThreadMarker};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::OverlayView;

/// Build the view the way `attach` does, store, replace and drop the app.
///
/// **A real `AppHandle`, stored twice**, so the second assignment drops a
/// live handle: that `Some(AppHandle)` drop is the exact path the first
/// browser Open segfaulted on before the initializer called `set_ivars`.
pub fn the_overlay_view_ivars_hold(app: tauri::AppHandle) -> Result<(), String> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "the overlay check must run on the process main thread".to_owned())?;
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(8.0, 8.0));
    let view = OverlayView::new_with_frame(mtm.alloc::<OverlayView>(), frame);
    view.set_app(app.clone());
    view.set_app(app);
    // The Cmd+W read path, over the handle just stored.
    view.lower_surface();
    {
        let ivars = view.ivars();
        let slot = ivars.app.lock().map_err(|why| format!("the app slot: {why}"))?;
        if slot.is_none() {
            return Err("the app was not stored in the initialized ivar".to_owned());
        }
    }
    // The dealloc now drops the slot: the handle's drop glue runs here.
    drop(view);
    Ok(())
}

/// **The key methods, on a real view, emitting to a real app.**
///
/// The decision itself is a `#[test]` in `surface.rs`; what only the main
/// thread can show is that `flagsChanged:` and the chord path actually reach
/// the app as the two events the page listens for, with the payload it reads.
/// The events are AppKit's own synthesized key events, so the methods see
/// exactly what a real keypress hands them; the page's half is
/// `shell.ts`'s and is pinned in its own test.
pub fn the_overlay_view_forwards_its_keys(app: tauri::AppHandle) -> Result<(), String> {
    use std::sync::{Arc, Mutex};

    use objc2::rc::Retained;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags as Flags, NSEventType};
    use objc2_foundation::NSString;
    use tauri::Listener as _;

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "the overlay key check must run on the process main thread".to_owned())?;
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(8.0, 8.0));
    let view = OverlayView::new_with_frame(mtm.alloc::<OverlayView>(), frame);
    view.set_app(app.clone());

    // What the page would hear, as `event name, payload`.
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    for name in ["browser_modifier", "browser_modifier_chord"] {
        let heard = Arc::clone(&seen);
        app.listen(name, move |event| {
            if let Ok(mut heard) = heard.lock() {
                heard.push(format!("{name} {}", event.payload()));
            }
        });
    }

    let key = |kind: NSEventType,
               code: u16,
               flags: Flags,
               text: &str|
     -> Result<Retained<NSEvent>, String> {
        let characters = NSString::from_str(text);
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            kind,
            NSPoint::new(0.0, 0.0),
            flags,
            0.0,
            0,
            None,
            &characters,
            &characters,
            false,
            code,
        )
        .ok_or_else(|| "AppKit returned no synthesized event".to_owned())
    };
    let flags_changed = |code: u16, flags: Flags| -> Result<(), String> {
        let event = key(NSEventType::FlagsChanged, code, flags, "")?;
        // SAFETY: the responder reads only the event's keyCode and flags.
        let _: () = unsafe { objc2::msg_send![&view, flagsChanged: &*event] };
        Ok(())
    };

    // One Command held, the other pressed and released: the aggregate flag is
    // set at every step, so only per-side state gets the release right.
    flags_changed(55, Flags::Command)?;
    flags_changed(54, Flags::Command)?;
    flags_changed(54, Flags::Command)?;
    flags_changed(55, Flags::empty())?;
    // A key under the held Command, through BOTH responder entries: AppKit
    // routes a Command chord to `performKeyEquivalent:` before `keyDown:`,
    // and the page has to hear the chord from either.
    let typed = key(NSEventType::KeyDown, 8, Flags::Command, "c")?;
    // SAFETY: the responder reads the event and forwards it to the page.
    let _: () = unsafe { objc2::msg_send![&view, keyDown: &*typed] };
    let equivalent = key(NSEventType::KeyDown, 8, Flags::Command, "c")?;
    // SAFETY: the same event through the key-equivalent route; the view has
    // no window here, so the route defers to the superclass after the signal.
    let _: objc2::runtime::Bool =
        unsafe { objc2::msg_send![&view, performKeyEquivalent: &*equivalent] };

    let heard = seen.lock().map_err(|why| format!("the heard events: {why}"))?.clone();
    let want = [
        r#"browser_modifier {"code":55,"down":true}"#,
        r#"browser_modifier {"code":54,"down":true}"#,
        r#"browser_modifier {"code":54,"down":false}"#,
        r#"browser_modifier {"code":55,"down":false}"#,
        "browser_modifier_chord null",
        "browser_modifier_chord null",
    ];
    if heard != want {
        return Err(format!("the page heard {heard:?}, wanted {want:?}"));
    }
    drop(view);
    Ok(())
}
