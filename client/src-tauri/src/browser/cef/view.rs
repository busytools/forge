//! The takeover's view: the client's own browser, rendered natively.
//!
//! **Not a picture of a browser.** The browser CEF creates here is a child
//! NSView of the client's own window, painted by Chromium at the display's
//! own rate, taking keyboard, mouse and selection natively - nothing is
//! streamed and nothing is relayed. It is hidden except while a takeover is
//! up; between them the sessions drive the same browser through its CDP,
//! which is what "always headless until asked" means here.
//!
//! **Main thread only.** Every CEF call in this module must happen on the
//! thread that made the application - the takeover commands reach it through
//! `run_on_main_thread`, and setup already runs there.

use std::cell::RefCell;
use std::ffi::c_void;

use cef::*;

/// The takeover's bar height, in points: the HTML bar the view sits under.
/// The sheet is the other half of this number (`.takeover .bar` in
/// `client/src/assets/web.css`).
const BAR_PX: f64 = 44.0;

// Every callback the view needs is a default; the life-span handler exists
// so a stray close request tears nothing down behind the host's back.
wrap_client! {
    struct ViewClient {}

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(ViewLifeSpanHandler::new())
        }
    }
}

wrap_life_span_handler! {
    struct ViewLifeSpanHandler {}

    impl LifeSpanHandler {
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            // The browser outlives every takeover: a close request proceeds
            // only on the app's own terms.
            0
        }
    }
}

// The one view this client has.
thread_local! {
    static VIEW: RefCell<Option<View>> = const { RefCell::new(None) };
}

struct View {
    browser: Browser,
}

impl View {
    fn set_visible(&self, visible: bool) {
        let Some(host) = self.browser.host() else { return };
        // **AppKit's own hidden flag is what holds.** CEF's `hidden` at
        // creation and `was_hidden` alone left the view covering the client
        // window - a blank dark about:blank over the whole UI - so the view
        // is hidden the way any NSView is, with CEF told as well so its
        // renderer knows.
        let view = host.window_handle();
        if !view.is_null() {
            // SAFETY: CEF's own child view, on the main thread.
            unsafe {
                let ns_view = view.cast::<objc2_app_kit::NSView>();
                let hidden = objc2::runtime::Bool::new(!visible);
                let _: () = objc2::msg_send![ns_view, setHidden: hidden];
            }
        }
        host.was_hidden(i32::from(!visible));
    }

    fn set_bounds(&self, bounds: Rect) {
        let Some(host) = self.browser.host() else { return };
        let view = host.window_handle();
        if !view.is_null() {
            // SAFETY: CEF hands back the NSView it created inside the view
            // we gave it, on the main thread, and the frame is set in the
            // parent's own coordinates - the same ones the bounds came in.
            unsafe {
                let ns_view = view.cast::<objc2_app_kit::NSView>();
                let frame = objc2_foundation::NSRect::new(
                    objc2_foundation::NSPoint::new(bounds.x as f64, bounds.y as f64),
                    objc2_foundation::NSSize::new(
                        f64::from(bounds.width),
                        f64::from(bounds.height),
                    ),
                );
                let _: () = objc2::msg_send![ns_view, setFrame: frame];
            }
        }
        host.was_resized();
    }
}

/// Create the browser into `parent` (the client window's own NSView),
/// hidden, and keep it for the app's life.
pub fn install(parent: *mut c_void, width: f64, height: f64) {
    if VIEW.with(|slot| slot.borrow().is_some()) {
        return;
    }
    let bounds = bounds_for(width, height);
    let mut client = ViewClient::new();
    // `set_as_child` makes it visible; the client's default is hidden until
    // a takeover asks.
    let mut info = WindowInfo::default().set_as_child(parent, &bounds);
    info.hidden = 1;
    let created = browser_host_create_browser_sync(
        Some(&info),
        Some(&mut client),
        Some(&CefString::from("about:blank")),
        Some(&BrowserSettings::default()),
        None,
        None,
    );
    match created {
        Some(browser) => {
            let view = View { browser };
            // **Hidden AFTER creation, not just at it**: the creation flag
            // alone left a blank about:blank view covering the whole window,
            // which reads as a client that opened nothing.
            view.set_visible(false);
            VIEW.with(|slot| *slot.borrow_mut() = Some(view));
            eprintln!("forge client: the browser view is up (hidden)");
        }
        None => eprintln!("forge client: the browser view was not created"),
    }
}

/// Show or hide the view: the takeover's own door.
pub fn set_visible(visible: bool) {
    VIEW.with(|slot| {
        if let Some(view) = slot.borrow().as_ref() {
            view.set_visible(visible);
        }
    });
}

/// The window changed size: the view keeps the bar's height clear at the
/// top, whatever the window does.
pub fn resize(width: f64, height: f64) {
    VIEW.with(|slot| {
        if let Some(view) = slot.borrow().as_ref() {
            view.set_bounds(bounds_for(width, height));
        }
    });
}

/// The area the view fills: everything under the bar. macOS view coordinates
/// are bottom-left, so the bar at the visual top is the LAST `BAR_PX` of
/// height - the view starts at y=0 and stops short of it.
fn bounds_for(width: f64, height: f64) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: width as i32,
        height: (height - BAR_PX).max(0.0) as i32,
    }
}
