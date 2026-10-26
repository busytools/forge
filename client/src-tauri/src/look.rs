//! A look-only driver for the built bundle.
//!
//! **The agent that tests this machine has neither Accessibility nor Screen
//! Recording**, so it cannot click this app's window or capture it from
//! outside. Built with the `look-driver` feature and handed a socket path in
//! `FORGE_LOOK_SOCKET`, the app drives itself for a look: its window's full
//! screen state, JavaScript in its own page, the windows it owns (a picker's
//! panel included), and a screenshot of its own window - an app may capture
//! its own windows without Screen Recording. Production bundles never
//! compile this module.

#![allow(unsafe_code, reason = "AppKit and CoreGraphics are its window and its pixels")]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};

use objc2::AnyThread as _;
use objc2_foundation::NSSize;
use serde_json::{Value, json};
use tauri::Manager as _;

/// Run the driver when the feature is built.
///
/// **The socket path is the env's, or the look rig's fixed one.** A bundle
/// launched through LaunchServices (`open`) carries no environment of its
/// own, and that launch is the one that runs in the person's GUI session -
/// where a file panel can actually open. The shipping bundle never builds
/// this feature.
pub fn init(app: &tauri::AppHandle) {
    let path =
        std::env::var("FORGE_LOOK_SOCKET").unwrap_or_else(|_| "/tmp/forge-look.sock".to_owned());
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(why) => {
            tauri_plugin_log::log::warn!("the look driver's socket failed ({path}): {why}");
            return;
        }
    };
    let app = app.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let app = app.clone();
            std::thread::spawn(move || serve(&app, stream));
        }
    });
    tauri_plugin_log::log::info!("the look driver listens on {path}");
}

fn serve(app: &tauri::AppHandle, stream: UnixStream) {
    let Ok(read) = stream.try_clone() else { return };
    let mut write = stream;
    for line in BufReader::new(read).lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let reply = answer(app, &request);
        if writeln!(write, "{reply}").is_err() {
            break;
        }
        let _ = write.flush();
    }
}

fn answer(app: &tauri::AppHandle, request: &Value) -> Value {
    let Some(window) = app.get_webview_window("main") else {
        return json!({ "error": "the client has no main window" });
    };
    match request["cmd"].as_str().unwrap_or_default() {
        "fullscreen" => {
            let on = request["on"].as_bool().unwrap_or(false);
            match window.set_fullscreen(on) {
                Ok(()) => json!({ "ok": true }),
                Err(why) => json!({ "error": why.to_string() }),
            }
        }
        // **The windowed alternative to full screen**: AppKit's zoom, which
        // is what the green button's option-click and a title-bar
        // double-click do without leaving the desktop.
        "zoom" => {
            let Ok(pointer) = window.ns_window() else {
                return json!({ "error": "the window has no NSWindow" });
            };
            let address = pointer as usize;
            on_main(app, move || {
                let Some(_main) = objc2::MainThreadMarker::new() else {
                    return json!({ "error": "not the main thread" });
                };
                // SAFETY: the pointer is the main window's NSWindow, read on
                // the main thread while the window lives.
                let window = unsafe { &*(address as *const objc2_app_kit::NSWindow) };
                window.zoom(None);
                json!({ "ok": true })
            })
        }
        "size" => {
            let width = request["w"].as_f64().unwrap_or(1440.0);
            let height = request["h"].as_f64().unwrap_or(900.0);
            match window.set_size(tauri::LogicalSize::new(width, height)) {
                Ok(()) => json!({ "ok": true }),
                Err(why) => json!({ "error": why.to_string() }),
            }
        }
        // **The green button's own transition**, not Tauri's: AppKit's
        // `toggleFullScreen:` moves the window to its own Space, which is the
        // path the person takes and the one the bar report is about.
        "native-fullscreen" => {
            let Ok(pointer) = window.ns_window() else {
                return json!({ "error": "the window has no NSWindow" });
            };
            let address = pointer as usize;
            on_main(app, move || {
                let Some(_main) = objc2::MainThreadMarker::new() else {
                    return json!({ "error": "not the main thread" });
                };
                // SAFETY: the pointer is the main window's NSWindow, read on
                // the main thread while the window lives.
                let window = unsafe { &*(address as *const objc2_app_kit::NSWindow) };
                window.toggleFullScreen(None);
                json!({ "ok": true })
            })
        }
        "eval" => {
            let js = request["js"].as_str().unwrap_or_default();
            eval_js(app, js)
        }
        "pick" => {
            let id = request["id"].as_u64().unwrap_or(0);
            let paths: Vec<std::path::PathBuf> = request["paths"]
                .as_array()
                .map(|paths| {
                    paths.iter().filter_map(Value::as_str).map(std::path::PathBuf::from).collect()
                })
                .unwrap_or_default();
            let Some(bridge) = app.try_state::<std::sync::Arc<crate::bridge::Bridge>>() else {
                return json!({ "error": "the bridge is not up" });
            };
            let bridge = std::sync::Arc::clone(&bridge);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let paths = paths.into_iter().map(tauri_plugin_fs::FilePath::Path).collect();
                bridge.answer_paths(&app, id, paths).await;
            });
            json!({ "ok": true })
        }
        // **A real click through the app's own event queue**, at a point in
        // the browser surface's own coordinates: the overlay's mouse handlers
        // turn it into the gesture the server injects, exactly as a person's
        // click does.
        "click-page" => {
            let x = request["x"].as_f64().unwrap_or(0.0);
            let y = request["y"].as_f64().unwrap_or(0.0);
            on_main(app, move || click_page(x, y))
        }
        "cancel-picker" => on_main(app, || {
            use objc2::rc::Retained;
            use objc2_app_kit::{NSApplication, NSSavePanel};

            let Some(_main) = objc2::MainThreadMarker::new() else {
                return json!({ "error": "not the main thread" });
            };
            let application = NSApplication::sharedApplication(_main);
            let mut cancelled = 0;
            for window in application.windows().iter() {
                let class = window.class().name().to_string_lossy().into_owned();
                if !(class.contains("OpenPanel") || class.contains("SavePanel")) {
                    continue;
                }
                // SAFETY: the window's own class is a panel's, and cancel is
                // its dismiss action.
                unsafe {
                    let panel = &*(Retained::as_ptr(&window).cast::<NSSavePanel>());
                    panel.cancel(None);
                }
                cancelled += 1;
            }
            json!({ "ok": true, "cancelled": cancelled })
        }),
        "windows" => on_main(app, windows),
        // **The picture's own gravity**, so a screenshot's composite (which
        // fits by hand) can be checked against what the layer really does.
        "overlay" => on_main(app, overlay_state),
        "shot" => {
            let path = request["path"].as_str().unwrap_or("/tmp/look.png");
            shot_window(app, path)
        }
        "quit" => {
            app.exit(0);
            json!({ "ok": true })
        }
        other => json!({ "error": format!("no command named {other}") }),
    }
}

/// Run one AppKit read or write on the main thread and carry its answer back.
fn on_main(app: &tauri::AppHandle, work: impl FnOnce() -> Value + Send + 'static) -> Value {
    let (tx, rx) = std::sync::mpsc::channel();
    if app
        .run_on_main_thread(move || {
            let _ = tx.send(work());
        })
        .is_err()
    {
        return json!({ "error": "the main thread would not take the work" });
    }
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| json!({ "error": "the main thread did not answer" }))
}

/// Post one mouse click at `(x, y)` in the browser surface's coordinates.
///
/// **The app's own event queue needs no Accessibility**: the event is handed
/// to this process, AppKit hit-tests the overlay, the overlay's `mouseDown:`
/// and `mouseUp:` run, and the gesture goes to the server the way the
/// person's does.
fn click_page(x: f64, y: f64) -> Value {
    use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSView};
    use objc2_foundation::NSPoint;

    let Some(_main) = objc2::MainThreadMarker::new() else {
        return json!({ "error": "not the main thread" });
    };
    let Some((view_address, _, _, _)) = crate::browser::cef::surface::look_overlay() else {
        return json!({ "error": "the browser surface is not laid" });
    };
    // SAFETY: main thread, and the view is the slot's own.
    let view = unsafe { &*(view_address as *const NSView) };
    let Some(window) = view.window() else {
        return json!({ "error": "the surface is not in a window" });
    };
    let point = view.convertPoint_toView(NSPoint::new(x, y), None);
    let back = view.convertPoint_fromView(point, None);
    let frame = view.frame();
    // SAFETY: main thread, and the view is in its hierarchy.
    let superview = unsafe { view.superview() }.map(|view| view.bounds().size);
    let application = NSApplication::sharedApplication(_main);
    // **A click lands in the key window**, so bring the app forward the way
    // a person would before clicking.
    application.activate();
    for kind in [NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
        let Some(event) = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind,
            point,
            NSEventModifierFlags::empty(),
            0.0,
            window.windowNumber(),
            None,
            1,
            1,
            1.0,
        ) else {
            return json!({ "error": "the click event could not be made" });
        };
        application.postEvent_atStart(&event, false);
    }
    json!({
        "ok": true,
        "at": [point.x, point.y],
        "back": [back.x, back.y],
        "frame": [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height],
        "super": superview.map(|size| [size.width, size.height]),
    })
}

/// The overlay layer's own drawing state.
fn overlay_state() -> Value {
    let Some((view, frame, superview, viewport)) = crate::browser::cef::surface::look_overlay()
    else {
        return json!({ "error": "the surface is not laid" });
    };
    // SAFETY: main thread, and the slot holds the view while this runs.
    let view = unsafe { &*(view as *const objc2_app_kit::NSView) };
    let Some(layer) = view.layer() else {
        return json!({ "error": "the view has no layer" });
    };
    let gravity = layer.contentsGravity().to_string();
    // SAFETY: main thread, and the slot holds the view whose contents it is.
    let contents = unsafe { layer.contents() }.map(|_| true).unwrap_or(false);
    json!({
        "gravity": gravity,
        "contents": contents,
        "contents_scale": layer.contentsScale(),
        "frame": [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height],
        "super": [superview.width, superview.height],
        "viewport": [viewport.width, viewport.height],
        "inset": (superview.height - viewport.height).max(0.0),
    })
}

/// Every window this app owns, with the panel a picker raises among them.
fn windows() -> Value {
    use objc2_app_kit::NSApplication;

    let Some(_main) = objc2::MainThreadMarker::new() else {
        return json!({ "error": "not the main thread" });
    };
    let app = NSApplication::sharedApplication(_main);
    let mut rows = Vec::new();
    for window in app.windows().iter() {
        let frame = window.frame();
        rows.push(json!({
            "class": window.class().name().to_string_lossy(),
            "title": window.title().to_string(),
            "visible": window.isVisible(),
            "frame": [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height],
        }));
    }
    json!({ "windows": rows })
}

/// Run one expression in the app's own page and answer what it returned.
///
/// **A page written for the look returns `JSON.stringify(value)`**: the
/// completion carries objects as opaque pointers, so the page does the
/// encoding that crosses back.
fn eval_js(app: &tauri::AppHandle, js: &str) -> Value {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSError, NSString};
    use objc2_web_kit::WKWebView;

    let Some(window) = app.get_webview_window("main") else {
        return json!({ "error": "the client has no main window" });
    };
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let source = js.to_owned();
    if let Err(why) = window.with_webview(move |platform| {
        // SAFETY: the platform webview is a WKWebView on macOS, and this
        // closure runs on the main thread.
        unsafe {
            let webview = &*(platform.inner().cast::<WKWebView>());
            let done = tx.clone();
            let block = block2::RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
                let answer = if !error.is_null() {
                    json!({ "error": (*error).localizedDescription().to_string() })
                } else if value.is_null() {
                    Value::Null
                } else {
                    js_value(&*value)
                };
                let _ = done.send(answer);
            });
            webview
                .evaluateJavaScript_completionHandler(&NSString::from_str(&source), Some(&block));
        }
    }) {
        return json!({ "error": format!("the webview cannot be reached: {why}") });
    }
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| json!({ "error": "the page did not answer within ten seconds" }))
}

/// One JavaScript result as JSON, narrowed by its Objective-C type.
///
/// **The completion handler hands over an `AnyObject`, never a string.** A
/// number, a boolean and NSNull each arrive as their own class, and casting
/// a number to `NSString` aborted the process in `UTF8String` (SIGABRT,
/// 2026-10-24). A string keeps the old route: JSON text answers as JSON, any
/// other string as itself. A class this does not know is printed by name
/// rather than guessed at.
fn js_value(value: &objc2::runtime::AnyObject) -> Value {
    use objc2_foundation::{NSNumber, NSString};
    use std::ffi::CStr;

    if let Some(text) = value.downcast_ref::<NSString>() {
        let text = text.to_string();
        return serde_json::from_str(&text).unwrap_or(Value::String(text));
    }
    if let Some(number) = value.downcast_ref::<NSNumber>() {
        // SAFETY: `objCType` returns a NUL-terminated type encoding that
        // lives as long as the number.
        let encoded = unsafe { CStr::from_ptr(number.objCType().as_ptr()) };
        // **A boolean is an NSNumber too.** `c` and `B` are a C and a C++
        // bool's encodings; a JS number is `d` or an integer type.
        if matches!(encoded.to_bytes(), b"c" | b"B") {
            return Value::Bool(number.boolValue());
        }
        let number = number.doubleValue();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an integral double inside the JS safe-integer range"
        )]
        if number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0 {
            return Value::from(number as i64);
        }
        return serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number);
    }
    Value::String(format!("<{}>", value.class().name().to_string_lossy()))
}

/// One screenshot of this app's own window, written as PNG.
///
/// **CoreGraphics will not capture even our own window without Screen
/// Recording** (it answers a black image), so the picture is the app's own:
/// WebKit's snapshot of its page, with the browser's native surface drawn
/// over it where the layer really sits. That is the same two-layer
/// composition the screen shows.
fn shot_window(app: &tauri::AppHandle, path: &str) -> Value {
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSError;
    use objc2_web_kit::WKWebView;

    let Some(window) = app.get_webview_window("main") else {
        return json!({ "error": "the client has no main window" });
    };
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let path = path.to_owned();
    if let Err(why) = window.with_webview(move |platform| {
        // SAFETY: the platform webview is a WKWebView on macOS, and this
        // closure runs on the main thread.
        unsafe {
            let webview = &*(platform.inner().cast::<WKWebView>());
            let done = tx.clone();
            let path = path.clone();
            let block = block2::RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                let answer = if image.is_null() {
                    let why = if error.is_null() {
                        "WebKit returned no snapshot".to_owned()
                    } else {
                        (*error).localizedDescription().to_string()
                    };
                    json!({ "error": why })
                } else {
                    compose(&*image, &path)
                };
                let _ = done.send(answer);
            });
            webview.takeSnapshotWithConfiguration_completionHandler(None, &block);
        }
    }) {
        return json!({ "error": format!("the webview cannot be reached: {why}") });
    }
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| json!({ "error": "the snapshot did not arrive within ten seconds" }))
}

/// Draw the page and, over it, the browser's native surface at its frame.
fn compose(webview_image: &objc2_app_kit::NSImage, path: &str) -> Value {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSView};
    use objc2_core_graphics::CGImage;
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSString};

    let Some(_main) = objc2::MainThreadMarker::new() else {
        return json!({ "error": "not the main thread" });
    };
    let overlay = crate::browser::cef::surface::look_overlay();
    let size = overlay.as_ref().map_or_else(|| webview_image.size(), |(_, _, size, _)| *size);
    // **The snapshot is the page, not the window.** In a titled window the
    // page viewport is shorter than the content view, and drawing the
    // snapshot at the window's size stretched it - which hid a picture that
    // really did cover the bar.
    let page = overlay.as_ref().map_or(size, |(_, _, _, viewport)| *viewport);
    // SAFETY: the snapshot is live for this call, and the canvas is drawn and
    // encoded on this thread before it is released.
    let Some(snapshot) =
        (unsafe { Retained::retain(webview_image as *const NSImage as *mut NSImage) })
    else {
        return json!({ "error": "the page's snapshot could not be kept" });
    };
    let drawing = block2::RcBlock::new(move |_rect: NSRect| -> objc2::runtime::Bool {
        snapshot.drawInRect(NSRect::new(NSPoint::ZERO, page));
        if let Some((view_address, frame, _, _)) = overlay {
            // SAFETY: main thread, and the overlay view is the slot's own.
            let view = unsafe { &*(view_address as *const NSView) };
            // SAFETY: main thread, the view is alive, and the layer's
            // contents are the CGImage the surface itself drew.
            let contents = unsafe { view.layer().and_then(|layer| layer.contents()) };
            if let Some(contents) = contents {
                let image = unsafe { Retained::cast_unchecked::<CGImage>(contents) };
                // **The composite fits the picture the way the layer does**:
                // a frame from another surface keeps its shape and is
                // centred with bars, not stretched into the view.
                let natural = NSSize::new(
                    CGImage::width(Some(&image)) as f64,
                    CGImage::height(Some(&image)) as f64,
                );
                let scale =
                    (frame.size.width / natural.width).min(frame.size.height / natural.height);
                let drawn = NSSize::new(natural.width * scale, natural.height * scale);
                let picture = NSImage::initWithCGImage_size(NSImage::alloc(), &image, natural);
                picture.drawInRect(NSRect::new(
                    NSPoint::new(
                        frame.origin.x + (frame.size.width - drawn.width) / 2.0,
                        frame.origin.y + (frame.size.height - drawn.height) / 2.0,
                    ),
                    drawn,
                ));
            }
        }
        objc2::runtime::Bool::YES
    });
    let canvas = NSImage::imageWithSize_flipped_drawingHandler(size, false, &drawing);
    let Some(tiff) = canvas.TIFFRepresentation() else {
        return json!({ "error": "the canvas made no image data" });
    };
    let Some(rep) = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &tiff) else {
        return json!({ "error": "the canvas did not read back as a bitmap" });
    };
    // SAFETY: the rep is a live bitmap and PNG is one of its own types.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    };
    let Some(png) = png else {
        return json!({ "error": "the picture did not encode as PNG" });
    };
    if !png.writeToFile_atomically(&NSString::from_str(path), true) {
        return json!({ "error": format!("the PNG did not write to {path}") });
    }
    json!({ "ok": true, "path": path })
}

#[cfg(test)]
mod tests {
    use super::js_value;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSNumber, NSString};

    /// **A JS number is a number, not a string.** The completion handler
    /// hands over an `NSNumber`, and formatting one as an `NSString` aborted
    /// the app in `UTF8String` (SIGABRT, 2026-10-24); each type keeps itself.
    #[test]
    fn a_number_bool_and_string_keep_their_types() {
        let number = NSNumber::numberWithDouble(42.0);
        let number: &AnyObject = &number;
        assert_eq!(js_value(number), serde_json::json!(42));
        let boolean = NSNumber::numberWithBool(true);
        let boolean: &AnyObject = &boolean;
        assert_eq!(js_value(boolean), serde_json::json!(true));
        let text = NSString::from_str("hi");
        let text: &AnyObject = &text;
        assert_eq!(js_value(text), serde_json::json!("hi"));
    }
}
