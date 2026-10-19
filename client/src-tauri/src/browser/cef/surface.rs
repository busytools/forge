//! The takeover's picture and its input.
//!
//! The browser is windowless; its frames arrive in the shared ring. This
//! module lays a native view over the takeover's page area - the whole window
//! under the bar - draws the ring's latest frame into its layer, and turns
//! the view's mouse, wheel and key events into CDP input for the host's page.
//! The page under it reports that area ([`Rect`]) and keeps drawing its state
//! lines until the first frame lands.

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

use tauri::{Emitter as _, Manager as _};

use super::input::{self, InputEvent, MouseKind, Sender};
use super::osr::{MAX_HEIGHT, MAX_WIDTH, Ring};
use super::spawn::ActiveHost;

/// Where the page says its takeover's page area is, in CSS pixels from the
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
    /// **The mapping the layer is reading.** CoreAnimation releases a frame's
    /// image on its next transaction commit, so the ring stays mapped until
    /// the view is gone and a commit has had its turn; otherwise the bytes
    /// are unmapped under a live CGImage.
    ring: Arc<Ring>,
}

static OVERLAY: Mutex<Option<Overlay>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct OverlayIvars {
    sender: Mutex<Option<Sender>>,
    /// The app, for the one key that acts on the surface itself (Cmd+W).
    app: Mutex<Option<tauri::AppHandle>>,
}

impl Default for OverlayIvars {
    fn default() -> Self {
        Self { sender: Mutex::new(None), app: Mutex::new(None) }
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

        /// **Command chords arrive here first.** AppKit routes key
        /// equivalents through `performKeyEquivalent:` before `keyDown:`,
        /// and Tauri's default menu matches Cmd+W (Close Window) and the
        /// Edit menu's Cmd+A/X/C/V/Z even with no responder of its own. The
        /// surface claims those six while IT holds first responder: Cmd+W
        /// lowers the takeover, the editing chords go to the page over CDP,
        /// and everything else defers to the superclass, which is the app's
        /// menu - including when the DOM bar's feedback field has focus.
        #[unsafe(method(performKeyEquivalent:))]
        unsafe fn perform_key_equivalent(
            &self,
            event: &objc2_app_kit::NSEvent,
        ) -> objc2::runtime::Bool {
            let flags = event.modifierFlags();
            let route = route_equivalent(
                character_of(event).unwrap_or('\0'),
                flags.contains(objc2_app_kit::NSEventModifierFlags::Command),
                flags.contains(objc2_app_kit::NSEventModifierFlags::Shift),
                flags.contains(objc2_app_kit::NSEventModifierFlags::Option),
                flags.contains(objc2_app_kit::NSEventModifierFlags::Control),
                self.holds_keyboard(),
            );
            match route {
                Equivalent::Lower => {
                    self.lower_surface();
                    true.into()
                }
                Equivalent::Page(character) => {
                    self.send_command(character, modifiers(event));
                    true.into()
                }
                Equivalent::Defer => objc2::msg_send![super(self), performKeyEquivalent: event],
            }
        }

        #[unsafe(method(keyDown:))]
        unsafe fn key_down(&self, event: &objc2_app_kit::NSEvent) {
            let flags = event.modifierFlags();
            let command = flags.contains(objc2_app_kit::NSEventModifierFlags::Command);
            let shift = flags.contains(objc2_app_kit::NSEventModifierFlags::Shift);
            let option = flags.contains(objc2_app_kit::NSEventModifierFlags::Option);
            let control = flags.contains(objc2_app_kit::NSEventModifierFlags::Control);
            let keycode = event.keyCode();
            // A menu that does not consume an editing chord still leaves it
            // here, and it goes to the page only under the same rule the
            // equivalent route used: exactly Command, with no other
            // modifier. A chord with one is the app's, and the page hears
            // none of it.
            if command {
                if !shift
                    && !option
                    && !control
                    && let Some(character) = character_of(event)
                    && editing_key(character).is_some()
                {
                    self.send_command(character, modifiers(event));
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

    fn set_app(&self, app: tauri::AppHandle) {
        if let Ok(mut slot) = self.ivars().app.lock() {
            *slot = Some(app);
        }
    }

    /// **The keyboard's way out of the takeover.** The bar's buttons are in
    /// the webview and the overlay holds first responder, so Cmd+W lowers the
    /// surface and the page's bar then carries back-to-chat, Done and Not now.
    fn lower_surface(&self) {
        if let Some(app) = self.ivars().app.lock().ok().and_then(|slot| slot.clone()) {
            let _ = app.emit("browser_surface_lower", ());
        }
    }

    /// Whether this view is the window's first responder - the gate that
    /// keeps the bar's DOM feedback field on the webview's own menu editing.
    fn holds_keyboard(&self) -> bool {
        let responder = self
            .window()
            .and_then(|window| window.firstResponder())
            .as_deref()
            .map(|responder| responder as *const NSResponder as usize);
        same_responder(responder, self as *const Self as usize)
    }

    /// An editing chord to the page: pasteboard text for Cmd+V - the host has
    /// no native edit menu, and a widget cannot read the pasteboard itself -
    /// and a key pair for the rest.
    fn send_command(&self, character: char, modifiers: i32) {
        if character == 'v' {
            if let Some(text) = pasteboard_text() {
                self.send(InputEvent::Text(text));
            }
            return;
        }
        let Some((key, code, windows_vk)) = editing_key(character) else { return };
        self.send(InputEvent::Key { down: true, key, code, windows_vk, modifiers });
        self.send(InputEvent::Key { down: false, key, code, windows_vk, modifiers });
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
        // SAFETY: the ring's pixels outlive this image, and
        // `kCFAllocatorNull` is CF's "do not free these bytes" allocator - a
        // NULL deallocator asserts and aborts.
        let Some(data) = (unsafe {
            CFData::with_bytes_no_copy(
                None,
                pixels,
                (width * height * 4) as isize,
                objc2_core_foundation::kCFAllocatorNull,
            )
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

/// Whether the window's first responder IS this view. A seam because the
/// gate is what keeps the bar's DOM feedback field on the menu's own editing.
fn same_responder(responder: Option<usize>, view: usize) -> bool {
    responder.is_some_and(|responder| responder == view)
}

/// What a Command key equivalent does when the surface has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Equivalent {
    /// Lower the takeover: back to the chat, the hand-off still parked.
    Lower,
    /// Send this editing chord to the page.
    Page(char),
    /// Leave it to the app's menu.
    Defer,
}

/// **Key equivalents match characters, not physical keys.** `character` is
/// `charactersIgnoringModifiers`, lowercased: the menu's own matching is
/// character-based, so on Dvorak the keycode that is W on QWERTY is a comma
/// and must not lower anything.
///
/// **Only EXACTLY Command is the takeover's.** Cmd+Shift+W, Cmd+Option+W and
/// Cmd+Control+W belong to the app, as they do outside the takeover. And only
/// while the overlay holds first responder: with the bar's feedback field
/// focused the webview keeps its own menu editing, paste included.
pub fn route_equivalent(
    character: char,
    command: bool,
    shift: bool,
    option: bool,
    control: bool,
    focused: bool,
) -> Equivalent {
    if !focused || !command || shift || option || control {
        return Equivalent::Defer;
    }
    match character.to_ascii_lowercase() {
        'w' => Equivalent::Lower,
        character @ ('a' | 'x' | 'c' | 'v' | 'z') => Equivalent::Page(character),
        _ => Equivalent::Defer,
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
fn editing_key(character: char) -> Option<(&'static str, &'static str, i32)> {
    Some(match character.to_ascii_lowercase() {
        'a' => ("a", "KeyA", 65),
        'x' => ("x", "KeyX", 88),
        'c' => ("c", "KeyC", 67),
        'v' => ("v", "KeyV", 86),
        'z' => ("z", "KeyZ", 90),
        _ => return None,
    })
}

/// The character a key event names, as the menu would match it.
fn character_of(event: &objc2_app_kit::NSEvent) -> Option<char> {
    let characters = event.charactersIgnoringModifiers()?;
    characters.to_string().chars().next().map(|ch| ch.to_ascii_lowercase())
}

fn pasteboard_text() -> Option<String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    let pasteboard = NSPasteboard::generalPasteboard();
    // SAFETY: the constant is a static string reference AppKit owns.
    let text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) }?;
    let text = text.to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Open the surface over the takeover's page area: size the host's view,
/// place the overlay exactly at the page's rect, and start drawing the ring.
pub async fn attach(
    app: &tauri::AppHandle,
    host: Arc<ActiveHost>,
    rect: Rect,
) -> Result<(), String> {
    let rect = rect.sane();
    host.ring.request(
        paint_px(rect.width * raster(rect)),
        paint_px(rect.height * raster(rect)),
        raster(rect),
    );
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let cancel = Arc::new(AtomicBool::new(false));

    // The person's file picker is the client's own panel, asked for by the
    // input task when a click lands on a file input.
    let (panels, mut panel_requests) =
        tokio::sync::mpsc::unbounded_channel::<input::PanelRequest>();
    let panel_app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(request) = panel_requests.recv().await {
            let picked = pick_file(&panel_app).await;
            let _ = request.reply.send(picked);
        }
    });
    let sender = input::start(host.port, Arc::clone(&cancel), panels).await?;
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "the client window is not there".to_owned())?;
    let (placed_tx, placed_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let cancel_for_closure = Arc::clone(&cancel);
    let ring_for_view = Arc::clone(&host.ring);
    let app_for_view = app.clone();
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
                        overlay.set_app(app_for_view.clone());
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
                        // **The surface takes the keyboard on Open.** Waiting
                        // for the first mouseDown left the keys going to the
                        // hidden composer: a CAPTCHA code typed after Open
                        // landed in an invisible draft.
                        if let Some(window) = overlay.window() {
                            let responder: &NSResponder = &overlay;
                            window.makeFirstResponder(Some(responder));
                        }
                        let view = &*overlay as *const OverlayView as usize;
                        *OVERLAY.lock().expect("the overlay slot") = Some(Overlay {
                            view,
                            generation,
                            cancel: Arc::clone(&cancel_for_closure),
                            ring: Arc::clone(&ring_for_view),
                        });
                        // **The local reference goes; the hierarchy keeps
                        // one.** `addSubview` retained the view, so letting
                        // this Retained drop is what keeps a second reference
                        // from leaking per open.
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
    tauri_plugin_log::log::info!(
        "surface placed (event_name cef_surface_placed, rect {:.0}x{:.0} at {:.0},{:.0}, scale {})",
        rect.width,
        rect.height,
        rect.x,
        rect.y,
        rect.scale,
    );
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
        let mut checked = std::time::Instant::now();
        let mut last_focus = 1_u32;
        loop {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(4)).await;
            // **A dead or frozen host must not leave its last frame up with
            // no words.** The state is the host's own word, the process is
            // checked for exit, and the DevTools endpoint proves the message
            // loop still answers - a static page paints nothing, so frames
            // alone cannot tell a quiet page from a wedged host. The header
            // is read and dropped before the probe: a raw pointer cannot
            // cross the await.
            if checked.elapsed() >= std::time::Duration::from_secs(2) {
                checked = std::time::Instant::now();
                let (state, visible, focused) = {
                    let header = host.ring.header();
                    // SAFETY: the ring is mapped for the host's lifetime.
                    unsafe {
                        (
                            (*header).state.load(Ordering::Acquire),
                            (*header).visible.load(Ordering::Relaxed),
                            (*header).focused.load(Ordering::Relaxed),
                        )
                    }
                };
                if focused != last_focus {
                    last_focus = focused;
                    tauri_plugin_log::log::debug!(
                        "the browser host reports focus {focused} (event_name cef_surface_focus)"
                    );
                }
                let alive = super::spawn::alive(host.pid);
                let answers = alive && super::spawn::probe(host.port).await;
                if state == super::osr::STATE_FAILED || visible == 0 || !answers {
                    let why = if state == super::osr::STATE_FAILED {
                        "the browser could not start"
                    } else {
                        "the browser stopped answering"
                    };
                    let _ = app.emit("browser_surface_failed", why.to_owned());
                    detach(&app);
                    break;
                }
            }
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
    // The same raster the attach asks for; the view still moves in points.
    host.ring.request(
        paint_px(rect.width * raster(rect)),
        paint_px(rect.height * raster(rect)),
        raster(rect),
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
    tauri_plugin_log::log::debug!(
        "releasing the surface's ring (event_name cef_surface_released, ring {})",
        overlay.ring.name,
    );
    overlay.cancel.store(true, Ordering::Release);
    let view = overlay.view;
    let later = app.clone();
    let _ = app.run_on_main_thread(move || {
        // The slot is already empty, so no queued draw reaches the view; the
        // hierarchy still retains it until this call takes it out.
        // SAFETY: main thread, and the view is in the hierarchy.
        unsafe {
            let view = &*(view as *const OverlayView);
            // **The frame goes before the mapping does.** CoreAnimation
            // releases its image on the transaction commit at the end of this
            // turn; the ring stays mapped one turn longer, below.
            if let Some(layer) = view.layer() {
                layer.setContents(None);
            }
            let window = view.window();
            let parent = view.superview();
            view.removeFromSuperview();
            // **And the keyboard goes back to the page.** The overlay held
            // first responder; without this the composer keeps none either.
            if let (Some(window), Some(parent)) = (window, parent) {
                for sibling in parent.subviews().iter() {
                    let pointer = (&*sibling as *const NSView) as usize;
                    if pointer != view as *const OverlayView as usize {
                        let _ = window.makeFirstResponder(Some(&*sibling));
                        break;
                    }
                }
            }
        }
        // The ring's mapping drops after CoreAnimation has committed the
        // empty contents: the image references the mapped pixels until then.
        let _ = later.run_on_main_thread(move || drop(overlay));
    });
}

/// The person's file picker, in the CLIENT's process and window: the app they
/// are looking at, which is what gives the panel focus. The host cancels its
/// own dialog, so this is the only picker a click can raise.
async fn pick_file(app: &tauri::AppHandle) -> Option<Vec<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = app.run_on_main_thread(move || {
        let _ = tx.send(client_panel());
    });
    rx.await.ok().flatten()
}

fn client_panel() -> Option<Vec<String>> {
    use objc2::MainThreadMarker;
    use objc2::runtime::NSObjectProtocol as _;
    use objc2_app_kit::{NSApp, NSModalResponseOK, NSOpenPanel};

    let mtm = MainThreadMarker::new()?;
    // `activate` is macOS 14+; the selector is chosen at run time so the
    // 11.0 target still starts and the panel takes focus on older systems.
    let app = NSApp(mtm);
    unsafe {
        if app.respondsToSelector(objc2::sel!(activate)) {
            let _: () = objc2::msg_send![&*app, activate];
        } else {
            let _: () = objc2::msg_send![&*app, activateIgnoringOtherApps: true];
        }
    }
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    let mut paths = Vec::new();
    for url in panel.URLs().iter() {
        if let Some(path) = url.path() {
            paths.push(path.to_string());
        }
    }
    Some(paths)
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

/// The raster the surface asks for: 1.5x, or less when the takeover is
/// larger than the ring's 4K budget.
///
/// **A fixed 1.5 breaks input on a big display**: past 2560x1440 CSS the
/// paint size clamps, the page is laid out wider than the frame it is drawn
/// into, and clicks land off. Deriving the scale from the rect keeps the
/// paint exact at any size; the layout stays the CSS size either way.
fn raster(rect: Rect) -> f64 {
    let fit = (f64::from(MAX_WIDTH) / rect.width).min(f64::from(MAX_HEIGHT) / rect.height);
    // **Never above the display's own ratio** - a 1x screen gains nothing
    // from a 1.5x raster but pays for every pixel - and never above the 1.5
    // the probe measured as the legible ceiling.
    let ceiling = rect.scale.clamp(1.0, 1.5);
    fit.min(ceiling).clamp(0.25, 1.5)
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

    /// **The frame's bytes are the ring's, and the data must not free
    /// them.** With a NULL deallocator CFData takes ownership of memory it
    /// never allocated and the release aborts in libmalloc - the invalid
    /// free this pins. The bytes here are a stack array on purpose: on a
    /// regression the release aborts inside this test rather than quietly
    /// freeing a heap buffer that happened to be valid.
    #[test]
    fn the_frame_data_does_not_free_the_rings_bytes() {
        let pixels = [0xAB_u8; 64 * 64 * 4];
        // SAFETY: the construction matches `draw_frame`: no-copy bytes the
        // caller keeps alive, and the null allocator as the deallocator.
        let data = unsafe {
            CFData::with_bytes_no_copy(
                None,
                pixels.as_ptr(),
                pixels.len() as isize,
                objc2_core_foundation::kCFAllocatorNull,
            )
        }
        .expect("a no-copy data");
        drop(data);
        assert_eq!(pixels[0], 0xAB, "the release left the ring's bytes alone");
    }

    /// **A takeover bigger than 2560x1440 CSS must not clamp the paint.**
    /// The scale comes down with the rect, so the ring's 4K budget is never
    /// hit and the page is never laid out wider than the frame it is drawn
    /// into - which is what kept clicks on their targets on a 6K display.
    #[test]
    fn a_large_takeover_raster_fits_the_ring() {
        let big = Rect { x: 0.0, y: 0.0, width: 3000.0, height: 1700.0, scale: 2.0 };
        let scale = raster(big);
        assert!(big.width * scale <= f64::from(MAX_WIDTH) + 1.0, "width fits the ring");
        assert!(big.height * scale <= f64::from(MAX_HEIGHT) + 1.0, "height fits the ring");
        assert!(scale < 1.5, "and comes down from the default to do it");
        let one_x = Rect { x: 0.0, y: 0.0, width: 900.0, height: 500.0, scale: 1.0 };
        assert_eq!(raster(one_x), 1.0, "a 1x display is never out-sampled");
    }

    /// Whether the window's responder IS this view - the gate that keeps the
    /// bar's DOM field on the menu's own editing.
    #[test]
    fn only_the_surface_itself_counts_as_having_the_keyboard() {
        assert!(same_responder(Some(7), 7));
        assert!(!same_responder(Some(8), 7), "the webview is not the surface");
        assert!(!same_responder(None, 7), "no responder, no keyboard");
    }

    /// **Cmd+W and the five editing chords are the takeover's, and only
    /// while it holds the keyboard.** The menu matches characters and exact
    /// modifiers, so the routing does too: a Dvorak comma is not W, and
    /// Cmd+Shift/Option/Control+W are the app's.
    #[test]
    fn the_takeover_routes_cmd_w_and_the_editing_chords_only_while_focused() {
        assert_eq!(route_equivalent('w', true, false, false, false, true), Equivalent::Lower);
        assert_eq!(
            route_equivalent('w', true, false, false, false, false),
            Equivalent::Defer,
            "the bar's field keeps the menu's own editing"
        );
        assert_eq!(
            route_equivalent(',', true, false, false, false, true),
            Equivalent::Defer,
            "Dvorak: the W keycode is a comma, and the menu matches characters"
        );
        assert_eq!(route_equivalent('w', true, true, false, false, true), Equivalent::Defer);
        assert_eq!(route_equivalent('w', true, false, true, false, true), Equivalent::Defer);
        assert_eq!(route_equivalent('w', true, false, false, true, true), Equivalent::Defer);
        for character in ['a', 'x', 'c', 'v', 'z'] {
            assert_eq!(
                route_equivalent(character, true, false, false, false, true),
                Equivalent::Page(character),
                "Cmd+{character} goes to the page"
            );
            assert_eq!(
                route_equivalent(character, true, false, false, false, false),
                Equivalent::Defer,
                "an unfocused surface leaves Cmd+{character} to the menu"
            );
            assert_eq!(
                route_equivalent(character, true, true, false, false, true),
                Equivalent::Defer,
                "Cmd+Shift+{character} is the app's"
            );
        }
        assert_eq!(route_equivalent('q', true, false, false, false, true), Equivalent::Defer);
    }

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
