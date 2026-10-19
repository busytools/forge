//! The column's picture and its input.
//!
//! The browser is windowless; its frames arrive in the shared ring. This
//! module lays a native view over the page's own conversation column, draws
//! the ring's latest frame into its layer, and turns the view's mouse, wheel
//! and key events into CDP input for the host's page. The page under it
//! reports where the column is ([`Rect`]) and keeps drawing its state lines
//! until the first frame lands.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Allocated;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{DefinedClass as _, MainThreadMarker, define_class, extern_methods};
use objc2_app_kit::{NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView};
use objc2_core_foundation::CFData;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use tauri::Manager as _;

use super::input::{self, InputEvent, MouseKind, Sender};
use super::spawn::ActiveHost;

/// Where the page says its conversation column is, in CSS pixels from the
/// viewport's top-left, and the device pixel ratio it is drawn at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

impl Rect {
    fn sane(self) -> Self {
        Self {
            x: self.x.max(0.0),
            y: self.y.max(0.0),
            width: self.width.max(1.0),
            height: self.height.max(1.0),
            scale: if self.scale > 0.0 { self.scale } else { 1.0 },
        }
    }
}

/// One open surface's native half.
struct Overlay {
    /// The view, held by AppKit; this is only an address on the main thread.
    view: usize,
    generation: u64,
    cancel: Arc<AtomicBool>,
}

static OVERLAY: Mutex<Option<Overlay>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct OverlayIvars {
    sender: Mutex<Option<Sender>>,
}

impl Default for OverlayIvars {
    fn default() -> Self {
        Self { sender: Mutex::new(None) }
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[ivars = OverlayIvars]
    struct OverlayView;

    impl OverlayView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            // Top-down, the page's own coordinate system: a local point is a
            // CSS pixel from the viewport's top-left.
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        unsafe fn mouse_down(&self, event: &objc2_app_kit::NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self as &NSResponder));
            }
            self.mouse(MouseKind::Press, event);
        }

        #[unsafe(method(mouseUp:))]
        unsafe fn mouse_up(&self, event: &objc2_app_kit::NSEvent) {
            self.mouse(MouseKind::Release, event);
        }

        #[unsafe(method(mouseDragged:))]
        unsafe fn mouse_dragged(&self, event: &objc2_app_kit::NSEvent) {
            self.mouse(MouseKind::Move, event);
        }

        #[unsafe(method(mouseMoved:))]
        unsafe fn mouse_moved(&self, event: &objc2_app_kit::NSEvent) {
            self.mouse(MouseKind::Move, event);
        }

        #[unsafe(method(scrollWheel:))]
        unsafe fn scroll_wheel(&self, event: &objc2_app_kit::NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let (x, y) = css_point(point);
            let event = InputEvent::Wheel {
                x,
                y,
                delta_x: -event.scrollingDeltaX(),
                delta_y: -event.scrollingDeltaY(),
                modifiers: modifiers(event),
            };
            self.send(event);
        }

        #[unsafe(method(keyDown:))]
        unsafe fn key_down(&self, event: &objc2_app_kit::NSEvent) {
            let flags = event.modifierFlags();
            let command = flags.contains(objc2_app_kit::NSEventModifierFlags::Command);
            let keycode = event.keyCode();
            // Cmd+V is a text paste: the host has no native edit menu, and a
            // widget cannot read the pasteboard itself.
            if command && keycode == 9 {
                if let Some(text) = pasteboard_text() {
                    self.send(InputEvent::Text(text));
                }
                return;
            }
            if command {
                if let Some((key, code, vk)) = command_key(keycode) {
                    self.send(InputEvent::Key {
                        down: true,
                        key,
                        code,
                        windows_vk: vk,
                        modifiers: modifiers(event),
                    });
                    self.send(InputEvent::Key {
                        down: false,
                        key,
                        code,
                        windows_vk: vk,
                        modifiers: modifiers(event),
                    });
                }
                return;
            }
            if let Some((key, code, vk)) = input::special_key(keycode) {
                self.send(InputEvent::Key {
                    down: true,
                    key,
                    code,
                    windows_vk: vk,
                    modifiers: modifiers(event),
                });
                self.send(InputEvent::Key {
                    down: false,
                    key,
                    code,
                    windows_vk: vk,
                    modifiers: modifiers(event),
                });
                return;
            }
            if let Some(characters) = event.characters() {
                let text = characters.to_string();
                if !text.is_empty() {
                    self.send(InputEvent::Text(text));
                }
            }
        }

        #[unsafe(method(keyUp:))]
        unsafe fn key_up(&self, _event: &objc2_app_kit::NSEvent) {
            // Text rides `insertText`; the specials were sent whole on down.
        }
    }

    unsafe impl NSObjectProtocol for OverlayView {}
);

impl OverlayView {
    extern_methods!(
        #[unsafe(method(initWithFrame:))]
        #[unsafe(method_family = init)]
        fn init_with_frame(this: Allocated<Self>, frame: NSRect) -> objc2::rc::Retained<Self>;
    );

    fn mouse(&self, kind: MouseKind, event: &objc2_app_kit::NSEvent) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let (x, y) = css_point(point);
        self.send(InputEvent::Mouse {
            kind,
            x,
            y,
            button: event.buttonNumber() as i32,
            clicks: event.clickCount() as i32,
            modifiers: modifiers(event),
        });
    }

    fn send(&self, event: InputEvent) {
        if let Some(sender) = self.ivars().sender.lock().ok().and_then(|slot| slot.clone()) {
            sender.send(event);
        }
    }

    fn set_sender(&self, sender: Sender) {
        if let Ok(mut slot) = self.ivars().sender.lock() {
            *slot = Some(sender);
        }
    }

    /// Draw the ring's latest frame. **The bytes are the ring's own**: a
    /// no-copy CFData and CGImage reference the shared mapping, so this side
    /// copies nothing - a frame at this size is tens of megabytes, and a copy
    /// per frame at 60 Hz is the difference between a live page and a
    /// stuttering one.
    ///
    /// # Safety
    ///
    /// `pixels` points at `width * height * 4` readable bytes that outlive
    /// the layer's use of them (the host's ring lives for its process).
    unsafe fn draw_frame(&self, pixels: *const u8, width: usize, height: usize) {
        let Some(layer) = self.layer() else { return };
        // SAFETY: the ring's pixels outlive this image; the null deallocator
        // says the bytes are not ours to free.
        let Some(data) = (unsafe {
            CFData::with_bytes_no_copy(None, pixels, (width * height * 4) as isize, None)
        }) else {
            return;
        };
        let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else { return };
        let Some(space) = CGColorSpace::new_device_rgb() else { return };
        // CEF paints BGRA: little-endian 32-bit with alpha first.
        let bitmap = CGBitmapInfo(
            CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
        );
        // SAFETY: the buffer is a full BGRA frame of exactly this size.
        let image = unsafe {
            CGImage::new(
                width,
                height,
                8,
                32,
                width * 4,
                Some(&space),
                bitmap,
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        };
        let Some(image) = image else { return };
        let frame = self.frame();
        if frame.size.width > 0.0 {
            layer.setContentsScale(width as f64 / frame.size.width);
        }
        // SAFETY: CGImage and id share the object pointer; the layer retains it.
        let contents = unsafe {
            &*(objc2_core_foundation::CFRetained::as_ptr(&image).as_ptr().cast::<AnyObject>())
        };
        unsafe { layer.setContents(Some(contents)) };
    }
}

fn modifiers(event: &objc2_app_kit::NSEvent) -> i32 {
    use objc2_app_kit::NSEventModifierFlags as Flags;
    let flags = event.modifierFlags();
    input::cdp_modifiers(
        flags.contains(Flags::Option),
        flags.contains(Flags::Control),
        flags.contains(Flags::Command),
        flags.contains(Flags::Shift),
    )
}

/// A letter key with Command held: the browser's own edit shortcuts, which a
/// text insert cannot express.
fn command_key(keycode: u16) -> Option<(&'static str, &'static str, i32)> {
    Some(match keycode {
        0 => ("a", "KeyA", 65),
        7 => ("x", "KeyX", 88),
        8 => ("c", "KeyC", 67),
        9 => ("v", "KeyV", 86),
        6 => ("z", "KeyZ", 90),
        _ => return None,
    })
}

fn pasteboard_text() -> Option<String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    let pasteboard = NSPasteboard::generalPasteboard();
    // SAFETY: the constant is a static string reference AppKit owns.
    let text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) }?;
    let text = text.to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Open the surface over the column: size the host's view, place the overlay
/// exactly at the page's rect, and start drawing the ring.
pub async fn attach(
    app: &tauri::AppHandle,
    host: Arc<ActiveHost>,
    rect: Rect,
) -> Result<(), String> {
    let rect = rect.sane();
    host.ring.request(
        paint_px(rect.width * rect.scale),
        paint_px(rect.height * rect.scale),
        rect.scale,
    );
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let cancel = Arc::new(AtomicBool::new(false));

    let sender = input::start(host.port, Arc::clone(&cancel)).await?;
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "the client window is not there".to_owned())?;
    let (placed_tx, placed_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let cancel_for_closure = Arc::clone(&cancel);
    let scheduled = window.with_webview(move |platform| {
        // SAFETY: this runs on the main thread, and the platform view is the
        // webview's own NSView for the window's lifetime.
        let placed = unsafe {
            let webview = platform.inner().cast::<NSView>();
            match (webview.as_ref(), MainThreadMarker::new()) {
                (Some(webview), Some(mtm)) => match webview.superview() {
                    Some(superview) => {
                        let overlay = OverlayView::init_with_frame(
                            mtm.alloc::<OverlayView>(),
                            frame_for(&superview, rect),
                        );
                        overlay.set_sender(sender);
                        overlay.setWantsLayer(true);
                        superview.addSubview(&overlay);
                        let area = NSTrackingArea::initWithRect_options_owner_userInfo(
                            mtm.alloc::<NSTrackingArea>(),
                            overlay.bounds(),
                            NSTrackingAreaOptions::MouseMoved
                                | NSTrackingAreaOptions::ActiveInKeyWindow
                                | NSTrackingAreaOptions::InVisibleRect,
                            Some(&overlay),
                            None,
                        );
                        overlay.addTrackingArea(&area);
                        let view = &*overlay as *const OverlayView as usize;
                        *OVERLAY.lock().expect("the overlay slot") = Some(Overlay {
                            view,
                            generation,
                            cancel: Arc::clone(&cancel_for_closure),
                        });
                        // Retained by the view hierarchy from here.
                        std::mem::forget(overlay);
                        Ok(())
                    }
                    None => Err("the client window's content view is not there".to_owned()),
                },
                _ => Err("the browser surface must be placed on the main thread".to_owned()),
            }
        };
        let _ = placed_tx.send(placed);
    });
    if let Err(why) = scheduled {
        cancel.store(true, Ordering::Release);
        return Err(format!("the window's webview cannot be reached: {why}"));
    }
    // The placement runs on the main thread; a placement that never lands
    // must not hold the person's Open forever.
    match tokio::time::timeout(std::time::Duration::from_secs(3), placed_rx).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(why))) => {
            cancel.store(true, Ordering::Release);
            return Err(why);
        }
        _ => {
            cancel.store(true, Ordering::Release);
            return Err("the browser surface could not be placed over the conversation".to_owned());
        }
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut last = 0_u64;
        loop {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(8)).await;
            let header = host.ring.header();
            // SAFETY: the ring is mapped for the host's lifetime.
            let frame = unsafe { (*header).frame.load(Ordering::Acquire) };
            if frame == last || frame % 2 != 0 {
                continue;
            }
            last = frame;
            let width = unsafe { (*header).width.load(Ordering::Relaxed) } as usize;
            let height = unsafe { (*header).height.load(Ordering::Relaxed) } as usize;
            if width == 0 || height == 0 {
                continue;
            }
            let ring = Arc::clone(&host.ring);
            let _ = app.run_on_main_thread(move || {
                let slot = OVERLAY.lock().expect("the overlay slot");
                let Some(overlay) = slot.as_ref() else { return };
                if overlay.generation != generation {
                    return;
                }
                // SAFETY: the view lives while this generation is current,
                // both guarded on the main thread by the same slot; the ring
                // outlives the layer's use of its pixels.
                unsafe {
                    let view = &*(overlay.view as *const OverlayView);
                    view.draw_frame(ring.pixels(), width, height);
                }
            });
        }
    });
    Ok(())
}

/// Move and resize the surface; the host's view size follows.
pub fn set_rect(app: &tauri::AppHandle, host: &ActiveHost, rect: Rect) {
    let rect = rect.sane();
    host.ring.request(
        paint_px(rect.width * rect.scale),
        paint_px(rect.height * rect.scale),
        rect.scale,
    );
    let Some(generation) = OVERLAY.lock().ok().and_then(|slot| slot.as_ref().map(|o| o.generation))
    else {
        return;
    };
    let _ = app.run_on_main_thread(move || {
        let slot = OVERLAY.lock().expect("the overlay slot");
        let Some(overlay) = slot.as_ref() else { return };
        if overlay.generation != generation {
            return;
        }
        // SAFETY: the view lives while this generation is current.
        unsafe {
            let view = &*(overlay.view as *const OverlayView);
            if let Some(superview) = view.superview() {
                view.setFrame(frame_for(&superview, rect));
            }
        }
    });
}

/// Close the surface: stop drawing and take the overlay down.
pub fn detach(app: &tauri::AppHandle) {
    let overlay = OVERLAY.lock().ok().and_then(|mut slot| slot.take());
    let Some(overlay) = overlay else { return };
    overlay.cancel.store(true, Ordering::Release);
    let view = overlay.view;
    let _ = app.run_on_main_thread(move || {
        // The slot is already empty, so no queued draw reaches the view; the
        // hierarchy still retains it until this call takes it out.
        // SAFETY: main thread, and the view is in the hierarchy.
        unsafe {
            let view = &*(view as *const OverlayView);
            view.removeFromSuperview();
        }
    });
}

/// The overlay's frame in its superview's (bottom-left) coordinates.
fn frame_for(superview: &NSView, rect: Rect) -> NSRect {
    let bounds = superview.bounds();
    NSRect::new(
        NSPoint::new(rect.x, bounds.size.height - rect.y - rect.height),
        NSSize::new(rect.width, rect.height),
    )
}

fn paint_px(value: f64) -> u32 {
    value.round().clamp(1.0, f64::from(u32::MAX)) as u32
}

/// A point in the overlay's own (flipped) coordinates, in CSS pixels for the
/// page's viewport. **The mapping is 1:1**: the overlay is placed and sized
/// in points, and a point is the page's CSS pixel at the current zoom; the
/// device scale only lives in the frame buffer.
fn css_point(point: NSPoint) -> (f64, f64) {
    (point.x, point.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rect_with_no_size_still_has_a_pixel() {
        let rect = Rect { x: -5.0, y: -5.0, width: 0.0, height: 0.0, scale: 0.0 }.sane();
        assert_eq!(rect.x, 0.0);
        assert_eq!(rect.y, 0.0);
        assert!(rect.width >= 1.0);
        assert!(rect.height >= 1.0);
        assert_eq!(rect.scale, 1.0);
    }

    #[test]
    fn the_view_size_is_the_rect_in_device_pixels() {
        assert_eq!(paint_px(1100.0 * 2.0), 2200);
        assert_eq!(paint_px(0.0), 1);
    }

    #[test]
    fn a_point_maps_one_to_one_into_the_page() {
        // The overlay is placed in points, which are the page's CSS pixels:
        // the device scale lives in the buffer, never in the input mapping.
        assert_eq!(css_point(NSPoint::new(0.0, 0.0)), (0.0, 0.0));
        assert_eq!(css_point(NSPoint::new(13.5, 620.0)), (13.5, 620.0));
    }
}
