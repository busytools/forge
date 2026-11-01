//! The takeover's picture and its gestures.
//!
//! The browser lives on the server; its frames arrive encoded over the
//! socket. This module lays a native view over the takeover's page area - the
//! whole window under the bar - decodes each frame and draws it into the
//! view's layer, and turns the view's mouse, wheel and key events into the
//! gestures the server injects. The page under it reports that area
//! ([`Rect`]) and keeps drawing its state lines until the first frame lands.

#![allow(
    unsafe_code,
    clippy::ref_as_ptr,
    clippy::borrow_as_ptr,
    clippy::cast_possible_truncation,
    clippy::fn_params_excessive_bools,
    reason = "AppKit and CEF calls are the module's whole job: every message is an unsafe FFI \
              call, the pointers are CEF's own, and the casts are the C API's widths"
)]

use std::sync::{Arc, Mutex};

use forge_primitives::browser::{BrowserInputEvent, MouseAction};
use objc2::rc::Allocated;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{DefinedClass as _, MainThreadMarker, define_class, extern_methods, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::CFData;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::kCAGravityResizeAspect;
use tauri::{Emitter as _, Manager as _};

#[cfg(feature = "testing")]
pub mod testing;

use super::input;
use crate::browser::Rect;

/// One decoded frame, in the pixel layout CEF painted it.
pub struct Frame {
    /// The profile whose page this is, so a warning names it.
    pub profile: u16,
    /// The tab this picture belongs to, so a warning names it too.
    pub tab: u16,
    pub width: u32,
    pub height: u32,
    /// The page's own viewport, from the frame's header: the space a
    /// gesture's point is sent in.
    pub page_width: u32,
    pub page_height: u32,
    /// RGBA or RGB bytes, whichever the bitstream carried.
    pub pixels: Vec<u8>,
    /// Whether `pixels` carries an alpha channel (four bytes per pixel).
    pub alpha: bool,
}

/// Decode one WebP frame the server relayed.
///
/// **A frame whose pixels do not match the header's size is dropped**, not
/// stretched: the header is the host's own statement about the frame, and a
/// mismatch is a corrupt or truncated relay rather than a page to draw.
pub fn decode(
    profile: u16,
    tab: u16,
    webp: &[u8],
    width: u32,
    height: u32,
    page: (u32, u32),
) -> Option<Frame> {
    let image = webp::Decoder::new(webp).decode()?;
    if image.width() != width || image.height() != height {
        return None;
    }
    let pixels = image.to_vec();
    let alpha = match pixels.len() {
        len if len == (width as usize) * (height as usize) * 4 => true,
        len if len == (width as usize) * (height as usize) * 3 => false,
        _ => return None,
    };
    Some(Frame {
        profile,
        tab,
        width,
        height,
        page_width: page.0,
        page_height: page.1,
        pixels,
        alpha,
    })
}

/// One open surface's native half.
struct Overlay {
    /// The view, held by AppKit; this is only an address on the main thread.
    view: usize,
    /// The page's own viewport, for the look rig's composite: the snapshot
    /// it draws must be the page's size, not the window's.
    viewport: NSSize,
}

static OVERLAY: Mutex<Option<Overlay>> = Mutex::new(None);

/// The overlay slot, taken even from a poisoned lock: it holds an `Option`
/// and nothing it stores can be left half-written.
fn overlay_slot() -> std::sync::MutexGuard<'static, Option<Overlay>> {
    OVERLAY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// **The look driver's window onto the overlay.** The overlay's view address,
/// its frame in the window's content view, the content view's own size and
/// the page's viewport - a screenshot draws the page's snapshot at the page's
/// size, and the picture where it really is.
#[cfg(feature = "look-driver")]
pub fn look_overlay() -> Option<(usize, NSRect, NSSize, NSSize)> {
    let slot = OVERLAY.lock().ok()?;
    let overlay = slot.as_ref()?;
    // SAFETY: read on the main thread by the caller, while the slot holds it.
    unsafe {
        let view = &*(overlay.view as *const OverlayView);
        let superview = view.superview()?;
        Some((overlay.view, view.frame(), superview.bounds().size, overlay.viewport))
    }
}

struct OverlayIvars {
    /// The app, for the one key that acts on the surface itself (Cmd+W) and
    /// for the socket the gestures go back on.
    app: Mutex<Option<tauri::AppHandle>>,
    /// The picture this view is drawing: a gesture is mapped through it.
    picture: Mutex<Option<Picture>>,
    /// The webview the page's DOM lives in, so the pane chord can give it
    /// the keyboard. It lives for the window's own lifetime.
    webview: Mutex<Option<usize>>,
    /// Each forwarded modifier side's own state.
    ///
    /// **The aggregate `Command`/`Control` flag cannot tell the sides apart**:
    /// with one Command held, releasing the other still leaves `Command` set,
    /// so a direction read off the flag reports that release as a second
    /// press and the page never ends its take. Only this per-side state says
    /// which side changed and which way.
    modifiers: Mutex<ModifierMask>,
}

/// The forwarded modifier sides, one bit each.
type ModifierMask = u8;
const LEFT_COMMAND: ModifierMask = 1 << 0;
const RIGHT_COMMAND: ModifierMask = 1 << 1;
const LEFT_CONTROL: ModifierMask = 1 << 2;
const RIGHT_CONTROL: ModifierMask = 1 << 3;

/// The overlay's two key events, as the page listens for them.
///
/// **Both sides of the language boundary spell these literals**, so a rename
/// on either side alone kills desktop dictation with every other test green;
/// `the_overlay_events_are_the_names_the_page_listens_for` reads the page's
/// own listeners back.
const MODIFIER_EVENT: &str = "browser_modifier";
const MODIFIER_CHORD_EVENT: &str = "browser_modifier_chord";

/// Whose keyboard it is, pushed to the page when the pane chords move it:
/// the overlay's picture holds it, or the bar's DOM does.
const KEYBOARD_EVENT: &str = "browser_keyboard";

/// The two panes the keyboard moves between, as the page hears them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum KeyboardHolder {
    Bar,
    Page,
}

/// [`KEYBOARD_EVENT`]'s payload: the holder alone, so a page narrows one
/// field rather than guessing a state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
struct Keyboard {
    holder: KeyboardHolder,
}

/// **The pane chords are the platform's own**: exactly Command+Option with
/// an arrow, the two macOS Chrome moves between its toolbar and its page.
/// They are key equivalents, so the remote page never sees them.
fn pane_key(keycode: u16, command: bool, shift: bool, option: bool, control: bool) -> bool {
    command && option && !shift && !control && matches!(keycode, 125 | 126)
}

/// The drawn picture's two spaces: the frame's own pixels, and the page's
/// viewport they were painted from.
#[derive(Clone, Copy)]
struct Picture {
    pixels: (f64, f64),
    page: (f64, f64),
}

/// One bare modifier's change, as the webview reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
struct ModifierPress {
    /// The raw `keyCode`: 54 right Command, 55 left Command, 59 left Control,
    /// 62 right Control.
    code: u16,
    /// Whether that modifier is down after the change.
    down: bool,
}

/// **A bare modifier arrives as `flagsChanged:`, never as `keyDown:`.** The
/// shell carries the raw side and the direction and decides nothing about the
/// binding; the client owns which side is its key.
fn modifier_press(
    keycode: u16,
    flags: objc2_app_kit::NSEventModifierFlags,
    held: &mut ModifierMask,
) -> Option<ModifierPress> {
    use objc2_app_kit::NSEventModifierFlags as Flags;
    let (side, other, family) = match keycode {
        54 => (RIGHT_COMMAND, LEFT_COMMAND, Flags::Command),
        55 => (LEFT_COMMAND, RIGHT_COMMAND, Flags::Command),
        59 => (LEFT_CONTROL, RIGHT_CONTROL, Flags::Control),
        62 => (RIGHT_CONTROL, LEFT_CONTROL, Flags::Control),
        _ => return None,
    };
    let was = *held & side != 0;
    // **With the other side held the aggregate flag says only that the family
    // is still down**, so this side's own change is the direction; otherwise
    // the flag decides, which also re-syncs after an event the view never saw.
    let down =
        if flags.contains(family) && (*held & other != 0) { !was } else { flags.contains(family) };
    if down {
        *held |= side;
    } else {
        *held &= !side;
    }
    Some(ModifierPress { code: keycode, down })
}

/// **The other half of a chord.** While the overlay holds the keyboard the
/// page never sees the key that makes a forwarded modifier a chord, so a
/// Command/Control chord would leave the take its press began standing and
/// submit it on release. The shell reports only that a key arrived under a
/// held Command/Control; the page marks the press it already tracks.
fn chord_arrived(flags: objc2_app_kit::NSEventModifierFlags) -> bool {
    use objc2_app_kit::NSEventModifierFlags as Flags;
    flags.contains(Flags::Command) || flags.contains(Flags::Control)
}

/// The picture one drawn frame puts on screen.
fn picture_of(frame: &Frame) -> Picture {
    Picture {
        pixels: (f64::from(frame.width), f64::from(frame.height)),
        page: (f64::from(frame.page_width), f64::from(frame.page_height)),
    }
}

impl Default for OverlayIvars {
    fn default() -> Self {
        Self {
            app: Mutex::new(None),
            picture: Mutex::new(None),
            webview: Mutex::new(None),
            modifiers: Mutex::new(0),
        }
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[ivars = OverlayIvars]
    struct OverlayView;

    impl OverlayView {
        /// **objc2 requires an ivars class's initializer to call
        /// `set_ivars`.** Without it every slot read is uninitialized memory.
        #[unsafe(method_id(initWithFrame:))]
        unsafe fn init_with_frame(
            this: Allocated<Self>,
            frame: NSRect,
        ) -> objc2::rc::Retained<Self> {
            let this = this.set_ivars(OverlayIvars::default());
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }

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
                let held = self.holds_keyboard();
                window.makeFirstResponder(Some(self as &NSResponder));
                // **A click on the picture takes the keyboard back**, so a
                // ring the bar still draws is one the keys cannot reach.
                if !held {
                    self.emit(KEYBOARD_EVENT, Keyboard { holder: KeyboardHolder::Page });
                }
            }
            self.mouse(MouseAction::Press, event);
        }

        #[unsafe(method(mouseUp:))]
        unsafe fn mouse_up(&self, event: &objc2_app_kit::NSEvent) {
            self.mouse(MouseAction::Release, event);
        }

        #[unsafe(method(mouseDragged:))]
        unsafe fn mouse_dragged(&self, event: &objc2_app_kit::NSEvent) {
            self.mouse(MouseAction::Move, event);
        }

        #[unsafe(method(mouseMoved:))]
        unsafe fn mouse_moved(&self, event: &objc2_app_kit::NSEvent) {
            self.mouse(MouseAction::Move, event);
        }

        #[unsafe(method(scrollWheel:))]
        unsafe fn scroll_wheel(&self, event: &objc2_app_kit::NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let (x, y) = self.page_point_of(point);
            self.send(BrowserInputEvent::Wheel {
                x,
                y,
                delta_x: -event.scrollingDeltaX(),
                delta_y: -event.scrollingDeltaY(),
                modifiers: input::modifiers(event.modifierFlags()),
            });
        }

        /// **Command chords arrive here first.** AppKit routes key
        /// equivalents through `performKeyEquivalent:` before `keyDown:`,
        /// and Tauri's default menu matches Cmd+W (Close Window) and the
        /// Edit menu's Cmd+A/X/C/V/Z even with no responder of its own. The
        /// surface claims those eight while IT holds first responder: Cmd+W
        /// lowers the takeover, the editing chords go to the page, Cmd+[ and
        /// Cmd+] move through the page's history, and everything else defers
        /// to the superclass, which is the app's menu - including when the
        /// DOM bar's feedback field has focus.
        #[unsafe(method(performKeyEquivalent:))]
        unsafe fn perform_key_equivalent(
            &self,
            event: &objc2_app_kit::NSEvent,
        ) -> objc2::runtime::Bool {
            let flags = event.modifierFlags();
            if chord_arrived(flags) {
                self.chord_signal();
            }
            // **The pane chords move the keyboard, not the page.** Claimed
            // here, while this view holds it, so the remote page never sees
            // the two keys macOS Chrome keeps for the same move.
            if self.holds_keyboard()
                && pane_key(
                    event.keyCode(),
                    flags.contains(objc2_app_kit::NSEventModifierFlags::Command),
                    flags.contains(objc2_app_kit::NSEventModifierFlags::Shift),
                    flags.contains(objc2_app_kit::NSEventModifierFlags::Option),
                    flags.contains(objc2_app_kit::NSEventModifierFlags::Control),
                )
            {
                self.hand_keyboard_to_bar();
                return true.into();
            }
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
                    self.send_command(character, event.modifierFlags());
                    true.into()
                }
                Equivalent::Back => {
                    self.navigate(true);
                    true.into()
                }
                Equivalent::Forward => {
                    self.navigate(false);
                    true.into()
                }
                Equivalent::Defer => objc2::msg_send![super(self), performKeyEquivalent: event],
            }
        }

        #[unsafe(method(keyDown:))]
        unsafe fn key_down(&self, event: &objc2_app_kit::NSEvent) {
            let flags = event.modifierFlags();
            if chord_arrived(flags) {
                self.chord_signal();
            }
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
                    && input::editing_key(character).is_some()
                {
                    self.send_command(character, flags);
                }
                return;
            }
            if let Some((key, code, vk)) = input::special_key(keycode) {
                let modifiers = input::modifiers(flags);
                self.send(BrowserInputEvent::Key {
                    down: true,
                    key: key.to_owned(),
                    code: code.to_owned(),
                    windows_vk: vk,
                    modifiers,
                });
                self.send(BrowserInputEvent::Key {
                    down: false,
                    key: key.to_owned(),
                    code: code.to_owned(),
                    windows_vk: vk,
                    modifiers,
                });
                return;
            }
            if let Some(characters) = event.characters() {
                let text = characters.to_string();
                if !text.is_empty() {
                    self.send(BrowserInputEvent::Text { text });
                }
            }
        }

        #[unsafe(method(keyUp:))]
        unsafe fn key_up(&self, _event: &objc2_app_kit::NSEvent) {
            // Text rides `insertText`; the specials were sent whole on down.
        }

        /// **A bare modifier never reaches `keyDown:`.** AppKit turns it into
        /// `flagsChanged:`, and with no implementation here it was dropped
        /// before it reached either the page or the webview - so the
        /// push-to-talk key did nothing while the overlay held the keyboard.
        /// The raw side and direction go to the page, which owns the binding.
        #[unsafe(method(flagsChanged:))]
        unsafe fn flags_changed(&self, event: &objc2_app_kit::NSEvent) {
            let press = {
                let Ok(mut held) = self.ivars().modifiers.lock() else { return };
                modifier_press(event.keyCode(), event.modifierFlags(), &mut held)
            };
            if let Some(press) = press {
                self.emit(MODIFIER_EVENT, press);
            }
        }
    }

    unsafe impl NSObjectProtocol for OverlayView {}
);

impl OverlayView {
    extern_methods!(
        #[unsafe(method(initWithFrame:))]
        #[unsafe(method_family = init)]
        fn new_with_frame(this: Allocated<Self>, frame: NSRect) -> objc2::rc::Retained<Self>;
    );

    fn mouse(&self, action: MouseAction, event: &objc2_app_kit::NSEvent) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let (x, y) = self.page_point_of(point);
        self.send(BrowserInputEvent::Mouse {
            action,
            x,
            y,
            button: event.buttonNumber() as i32,
            clicks: event.clickCount() as i32,
            modifiers: input::modifiers(event.modifierFlags()),
        });
    }

    fn send(&self, event: BrowserInputEvent) {
        if let Some(app) = self.ivars().app.lock().ok().and_then(|slot| slot.clone()) {
            // **A gesture that never leaves this view is a dead page with no
            // line**; moves and wheels are a stream, so only the discrete
            // gestures are said at the level the client log keeps.
            let streaming = matches!(
                &event,
                BrowserInputEvent::Mouse { action: MouseAction::Move, .. }
                    | BrowserInputEvent::Wheel { .. }
            );
            if streaming {
                // **The guard is the allocation**: the facade admits `debug`
                // for the mic's target, so without it every move and wheel
                // would build its line before the filter drops it.
                if tauri_plugin_log::log::log_enabled!(tauri_plugin_log::log::Level::Debug) {
                    tauri_plugin_log::log::debug!(
                        "the overlay sent a gesture (event_name client_browser_input_sent, {})",
                        describe(&event)
                    );
                }
            } else {
                tauri_plugin_log::log::info!(
                    "the overlay sent a gesture (event_name client_browser_input_sent, {})",
                    describe(&event)
                );
            }
            crate::bridge::browser_input(&app, event);
        }
    }

    /// The arrows' keyboard route: the server owns the move.
    fn navigate(&self, back: bool) {
        if let Some(app) = self.ivars().app.lock().ok().and_then(|slot| slot.clone()) {
            crate::bridge::browser_nav(&app, back);
        }
    }

    fn set_app(&self, app: tauri::AppHandle) {
        if let Ok(mut slot) = self.ivars().app.lock() {
            *slot = Some(app);
        }
    }

    /// One point in the view, in the page's own CSS pixels.
    ///
    /// **The mapping is against the picture actually drawn**, so a resize, a
    /// re-lay or a frame for another surface's layout needs no bookkeeping
    /// here: the last drawn frame names both spaces.
    fn page_point_of(&self, point: NSPoint) -> (f64, f64) {
        let size = self.bounds().size;
        let picture = self.ivars().picture.lock().ok().and_then(|held| *held);
        match picture {
            Some(picture) => point_in_page(
                (point.x, point.y),
                (size.width, size.height),
                picture.pixels,
                picture.page,
            ),
            None => (point.x, point.y),
        }
    }

    /// **The keyboard's way out of the takeover.** The bar's buttons are in
    /// the webview and the overlay holds first responder, so Cmd+W lowers the
    /// surface and the page's bar then carries back-to-chat, Done and Not now.
    fn lower_surface(&self) {
        self.emit("browser_surface_lower", ());
    }

    /// **The keyboard moves into the bar's document.** The webview is where
    /// the page's DOM lives, so making it first responder is what lets the
    /// bar take the keys; the page is told in the same breath, because a
    /// focus ring it cannot reach is worse than none.
    fn hand_keyboard_to_bar(&self) {
        let webview = self.webview();
        let Some(webview) = webview else { return };
        if let Some(window) = self.window() {
            // SAFETY: main thread, and the webview lives for the window's
            // own lifetime.
            let view = unsafe { &*(webview as *const NSView) };
            window.makeFirstResponder(Some(view));
        }
        self.emit(KEYBOARD_EVENT, Keyboard { holder: KeyboardHolder::Bar });
    }

    fn set_webview(&self, webview: usize) {
        if let Ok(mut slot) = self.ivars().webview.lock() {
            *slot = Some(webview);
        }
    }

    /// The webview the page's DOM lives in, as the address it was attached
    /// with.
    fn webview(&self) -> Option<usize> {
        self.ivars().webview.lock().ok().and_then(|slot| *slot)
    }

    /// **The page marks its own press from this**, exactly as the DOM route
    /// marks it on the key it hears: the shell says only that a key arrived
    /// under a held Command/Control, and never which key or which binding.
    fn chord_signal(&self) {
        self.emit(MODIFIER_CHORD_EVENT, ());
    }

    /// Hand one event to the page, through the app this view was given.
    fn emit(&self, event: &'static str, payload: impl serde::Serialize + Clone) {
        if let Some(app) = self.ivars().app.lock().ok().and_then(|slot| slot.clone()) {
            let _ = app.emit(event, payload);
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

    /// An editing chord to the page: pasteboard text for Cmd+V - the page has
    /// no native edit menu, and a widget cannot read the pasteboard itself -
    /// and a key pair for the rest.
    fn send_command(&self, character: char, flags: objc2_app_kit::NSEventModifierFlags) {
        if character == 'v' {
            if let Some(text) = pasteboard_text() {
                self.send(BrowserInputEvent::Text { text });
            }
            return;
        }
        let Some((key, code, windows_vk)) = input::editing_key(character) else { return };
        let modifiers = input::modifiers(flags);
        self.send(BrowserInputEvent::Key {
            down: true,
            key: key.to_owned(),
            code: code.to_owned(),
            windows_vk,
            modifiers,
        });
        self.send(BrowserInputEvent::Key {
            down: false,
            key: key.to_owned(),
            code: code.to_owned(),
            windows_vk,
            modifiers,
        });
    }

    /// Draw one decoded frame. **The bytes are copied into the CFData**: a
    /// frame is a few megabytes and the copy is one memcpy, where handing
    /// CoreAnimation a borrowed buffer would free it under a live image when
    /// the next frame replaces this one.
    fn draw_frame(&self, frame: &Frame) {
        let Some(layer) = self.layer() else {
            tauri_plugin_log::log::debug!("client_browser_frame_no_layer");
            return;
        };
        // **Every frame is handed to CoreGraphics as RGBA.** A lossy WebP
        // frame decodes to three bytes a pixel, and `CGImage::new` refuses
        // `kCGImageAlphaNone` next to a 32-bit byte order - the image came
        // back nil and the surface stayed black (measured on the dev stack).
        let rgba: Vec<u8> = if frame.alpha {
            frame.pixels.clone()
        } else {
            let (pixels, _) = frame.pixels.as_chunks::<3>();
            let mut rgba = Vec::with_capacity(pixels.len() * 4);
            for pixel in pixels {
                rgba.extend_from_slice(pixel);
                rgba.push(0xFF);
            }
            rgba
        };
        let data = CFData::from_bytes(&rgba);
        let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else {
            tauri_plugin_log::log::debug!("client_browser_frame_no_provider");
            return;
        };
        let Some(space) = CGColorSpace::new_device_rgb() else {
            tauri_plugin_log::log::debug!("client_browser_frame_no_colour_space");
            return;
        };
        let bits = 32;
        // RGBA, straight alpha: bytes in memory are R,G,B,A.
        let bitmap = CGBitmapInfo(CGImageAlphaInfo::Last.0 | CGImageByteOrderInfo::Order32Big.0);
        let row = frame.width * 4;
        // SAFETY: the buffer is a full frame of exactly this size, and the
        // CFData owns a copy that outlives the image.
        let image = unsafe {
            CGImage::new(
                frame.width as usize,
                frame.height as usize,
                8,
                bits,
                row as usize,
                Some(&space),
                bitmap,
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        };
        let Some(image) = image else {
            tauri_plugin_log::log::warn!(
                "a browser frame could not become an image (event_name \
                 client_browser_frame_unpaintable, profile {}, tab {}, {}x{})",
                frame.profile,
                frame.tab,
                frame.width,
                frame.height
            );
            return;
        };
        let bounds = self.frame();
        if bounds.size.width > 0.0 {
            layer.setContentsScale(f64::from(frame.width) / bounds.size.width);
        }
        // **A frame keeps its own shape.** The page is laid out to the
        // controller's surface, so a watcher's surface can have another
        // aspect (a phone's portrait page on a desktop) and CoreAnimation's
        // default stretches it. Fitting it leaves bars, as the phone's canvas
        // does; only the controller sends input, and its frame matches.
        // SAFETY: the constant is QuartzCore's own immutable string.
        unsafe { layer.setContentsGravity(kCAGravityResizeAspect) };
        // SAFETY: CGImage and id share the object pointer; the layer retains it.
        let contents = unsafe {
            &*(objc2_core_foundation::CFRetained::as_ptr(&image).as_ptr().cast::<AnyObject>())
        };
        unsafe { layer.setContents(Some(contents)) };
        // **The gesture mapping follows the picture that is drawn**, so it is
        // taken from the same frame and only once the layer has it.
        if let Ok(mut held) = self.ivars().picture.lock() {
            *held = Some(picture_of(frame));
        }
    }
}

/// Whether the window's first responder IS this view.
fn same_responder(responder: Option<usize>, view: usize) -> bool {
    responder.is_some_and(|responder| responder == view)
}

/// One gesture, for the log.
fn describe(event: &BrowserInputEvent) -> String {
    match event {
        BrowserInputEvent::Mouse { action, x, y, .. } => {
            format!("mouse {action:?} at {x:.0},{y:.0}")
        }
        BrowserInputEvent::Wheel { x, y, delta_x, delta_y, .. } => {
            format!("wheel at {x:.0},{y:.0} by {delta_x:.0},{delta_y:.0}")
        }
        BrowserInputEvent::Key { down, key, .. } => {
            format!("key {} {key}", if *down { "down" } else { "up" })
        }
        BrowserInputEvent::Text { text } => format!("text {} bytes", text.len()),
    }
}

/// What a Command key equivalent does when the surface has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Equivalent {
    /// Lower the takeover: back to the chat, the hand-off still parked.
    Lower,
    /// Send this editing chord to the page.
    Page(char),
    /// Move the page back through its own history.
    Back,
    /// Move the page forward through its own history.
    Forward,
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
        '[' => Equivalent::Back,
        ']' => Equivalent::Forward,
        character @ ('a' | 'x' | 'c' | 'v' | 'z') => Equivalent::Page(character),
        _ => Equivalent::Defer,
    }
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

/// A point in the view, as the page's CSS pixels.
///
/// **The layer draws the picture fitted to the view (`resizeAspect`), so a
/// view's point is the page's only when the picture fills the view.** A page
/// laid out to another viewer's surface or to an agent's `browser_resize` is
/// letterboxed here, and the gesture maps through the rect it is drawn in,
/// then through the page's own viewport the frame names.
fn point_in_page(
    point: (f64, f64),
    bounds: (f64, f64),
    picture: (f64, f64),
    page: (f64, f64),
) -> (f64, f64) {
    if bounds.0 <= 0.0 || bounds.1 <= 0.0 || picture.0 <= 0.0 || picture.1 <= 0.0 {
        return (0.0, 0.0);
    }
    if page.0 <= 0.0 || page.1 <= 0.0 {
        return (0.0, 0.0);
    }
    let drawn = (bounds.0 / picture.0).min(bounds.1 / picture.1);
    let (width, height) = (picture.0 * drawn, picture.1 * drawn);
    let left = (bounds.0 - width) / 2.0;
    let top = (bounds.1 - height) / 2.0;
    ((point.0 - left) / width * page.0, (point.1 - top) / height * page.1)
}

/// The view's frame inside its superview, for a page area in CSS pixels.
///
/// **The page's viewport is the WEBVIEW, not its superview.** A titled
/// window's webview can sit inset inside the superview, and a rect measured
/// from the page's own top-left that skipped that inset started high enough
/// to cover the takeover's bar (measured on the dev stack: a 40 px bar was
/// under the picture).
fn frame_for(webview: &NSView, superview: &NSView, rect: Rect, inset: f64) -> NSRect {
    // **The page measures top-down and each view measures under its own
    // orientation.** The page area's top-left is converted as a POINT through
    // the window, then read back in the parent's orientation, so the picture
    // lands under the bar whichever view is flipped (a rect conversion
    // mirrored a whole-frame twice as often as it should: measured on the
    // dev stack, the picture covered the bar again).
    //
    // **`inset` is the chrome above the page**: page y 0 is that far below
    // the webview's own top in a titled window (zero in full screen).
    let bounds = webview.bounds().size;
    let top_left = if webview.isFlipped() {
        NSPoint::new(rect.x, inset + rect.y)
    } else {
        NSPoint::new(rect.x, bounds.height - inset - rect.y)
    };
    let in_window = webview.convertPoint_toView(top_left, None);
    let in_parent = superview.convertPoint_fromView(in_window, None);
    let origin = if superview.isFlipped() {
        in_parent
    } else {
        NSPoint::new(in_parent.x, in_parent.y - rect.height)
    };
    NSRect::new(origin, NSSize::new(rect.width, rect.height))
}

/// One superview rect back in the page's top-down coordinates, for the log.
fn page_y_of(webview: &NSView, rect: NSRect) -> f64 {
    page_y(webview.bounds().size.height, rect.origin.y, rect.size.height, webview.isFlipped())
}

/// One y between the page's top-down measure and a view's own measure.
///
/// **A view that is not flipped measures from the bottom**, so the mirror is
/// what keeps the picture under the bar instead of over it - and the same
/// mirror reads a placed rect back for the log.
fn page_y(height: f64, y: f64, rect_height: f64, flipped: bool) -> f64 {
    if flipped { y } else { height - y - rect_height }
}

/// Lay the overlay over the takeover's page area, or move it there.
///
/// **One view per surface, moved rather than rebuilt.** Every page-area
/// report used to lay a fresh view and remember only the newest, so the
/// older ones stayed in the hierarchy holding their last picture: going back
/// to the chat took one view down and left the paint over it (measured on
/// the dev stack).
///
/// **Placed on the main thread**: the view is AppKit's, and the call is made
/// from a command that already runs there.
pub fn attach(app: &tauri::AppHandle, rect: Rect, viewport: super::super::Viewport) {
    let rect = rect.sane();
    let viewport = viewport.sane();
    let app_for_view = app.clone();
    let Some(window) = app.get_webview_window("main") else {
        tauri_plugin_log::log::warn!("the client window is not there; the surface is not laid");
        return;
    };
    let scheduled = window.with_webview(move |platform| {
        // SAFETY: this runs on the main thread, and the platform view is the
        // webview's own NSView for the window's lifetime.
        unsafe {
            let webview = platform.inner().cast::<NSView>();
            match (webview.as_ref(), MainThreadMarker::new()) {
                (Some(webview), Some(mtm)) => match webview.superview() {
                    Some(superview) => {
                        // **The page's top is not the webview's top in a
                        // titled window.** The chrome above the page is the
                        // webview's height less the page viewport's, and a
                        // picture placed from page coordinates without it
                        // covers the bar by exactly that much.
                        let inset = (webview.bounds().size.height - viewport.height).max(0.0);
                        let placed = frame_for(webview, &superview, rect, inset);
                        // Read back in the page's own coordinates: the top
                        // must land on the page area's y, or the picture is
                        // over the bar.
                        let back = webview.convertRect_fromView(placed, Some(&superview));
                        let window = webview.window();
                        let window_frame = window.as_deref().map(objc2_app_kit::NSWindow::frame);
                        let screen = |rect: NSRect| -> String {
                            let Some(window) = window.as_deref() else { return "none".to_owned() };
                            let in_window = superview.convertRect_toView(rect, None);
                            let frame = window.convertRectToScreen(in_window);
                            format!(
                                "{},{} {}x{}",
                                frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
                            )
                        };
                        let overlay_screen = screen(placed);
                        let webview_screen = screen(webview.frame());
                        let webview_frame = webview.frame();
                        tauri_plugin_log::log::info!(
                            "the picture is laid (event_name client_browser_surface_attached, \
                             asked y {}, landed y {}, viewport inset {inset}, h {}, flipped {}, \
                             placed at {},{},{}x{}, super flipped {}, super {}x{}, window {}, \
                             overlay screen {}, webview screen {}, webview frame {},{},{}x{})",
                            rect.y,
                            page_y_of(webview, back) - inset,
                            back.size.height,
                            webview.isFlipped(),
                            placed.origin.x,
                            placed.origin.y,
                            placed.size.width,
                            placed.size.height,
                            superview.isFlipped(),
                            superview.bounds().size.width,
                            superview.bounds().size.height,
                            window_frame.map_or_else(
                                || "none".to_owned(),
                                |frame| format!(
                                    "{},{} {}x{}",
                                    frame.origin.x,
                                    frame.origin.y,
                                    frame.size.width,
                                    frame.size.height
                                )
                            ),
                            overlay_screen,
                            webview_screen,
                            webview_frame.origin.x,
                            webview_frame.origin.y,
                            webview_frame.size.width,
                            webview_frame.size.height
                        );
                        let here = &*superview as *const NSView;
                        // Bound outside the `if let`: the lock's guard lives
                        // through that body, and taking it again inside would
                        // deadlock.
                        let current =
                            OVERLAY.lock().ok().and_then(|slot| slot.as_ref().map(|o| o.view));
                        if let Some(current) = current {
                            let view = &*(current as *const OverlayView);
                            let same =
                                view.superview().as_deref().map(|each| each as *const NSView)
                                    == Some(here);
                            if same {
                                view.setFrame(placed);
                                if let Ok(mut slot) = OVERLAY.lock()
                                    && let Some(overlay) = slot.as_mut()
                                {
                                    overlay.viewport = NSSize::new(viewport.width, viewport.height);
                                }
                                return;
                            }
                            // A view from a window that is gone: take it down
                            // rather than leave it drawing nowhere.
                            view.removeFromSuperview();
                            *overlay_slot() = None;
                        }
                        let overlay =
                            OverlayView::new_with_frame(mtm.alloc::<OverlayView>(), placed);
                        overlay.set_app(app_for_view.clone());
                        overlay.setWantsLayer(true);
                        // **The picture tracks the window between reports.**
                        // A resize moves the webview with its own autoresizing
                        // while this view keeps whatever frame it was last
                        // given; a page report usually follows, but one that
                        // lands before the layout settles leaves the picture
                        // over the bar. Both margins stay fixed, and the layer
                        // clips its own image, so nothing outgrows its frame.
                        overlay.setAutoresizingMask(
                            NSAutoresizingMaskOptions::ViewWidthSizable
                                | NSAutoresizingMaskOptions::ViewHeightSizable,
                        );
                        if let Some(layer) = overlay.layer() {
                            layer.setMasksToBounds(true);
                        }
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
                        // landed in an invisible draft. **And the bar is told
                        // the page has it**, so a focus it still holds from
                        // before this attach is given up with the keys.
                        overlay.set_webview(webview as *const NSView as usize);
                        if let Some(window) = overlay.window() {
                            let responder: &NSResponder = &overlay;
                            window.makeFirstResponder(Some(responder));
                        }
                        overlay.emit(KEYBOARD_EVENT, Keyboard { holder: KeyboardHolder::Page });
                        let view = &*overlay as *const OverlayView as usize;
                        *overlay_slot() = Some(Overlay {
                            view,
                            viewport: NSSize::new(viewport.width, viewport.height),
                        });
                        // **The local reference goes; the hierarchy keeps
                        // one.** `addSubview` retained the view, so letting
                        // this Retained drop is what keeps a second reference
                        // from leaking per open.
                    }
                    None => tauri_plugin_log::log::warn!(
                        "the client window's content view is not there; the surface is not laid"
                    ),
                },
                _ => tauri_plugin_log::log::warn!(
                    "the browser surface must be placed on the main thread"
                ),
            }
        }
    });
    if let Err(why) = scheduled {
        tauri_plugin_log::log::warn!("the window's webview cannot be reached: {why}");
    }
}

/// Draw one decoded frame on the main thread.
pub fn draw(app: &tauri::AppHandle, frame: &Arc<Frame>) {
    let frame = Arc::clone(frame);
    let _ = app.run_on_main_thread(move || {
        let slot = overlay_slot();
        let Some(overlay) = slot.as_ref() else { return };
        // SAFETY: the view lives while the slot holds it, both on the main
        // thread.
        unsafe {
            let view = &*(overlay.view as *const OverlayView);
            view.draw_frame(&frame);
        }
    });
}

/// **Hand the keyboard to one of the two panes.** The bar's DOM lives in the
/// webview; the picture is this module's own view. With no overlay there is
/// nothing to move, which is a failed surface: the webview already has the
/// keys.
fn focus(app: &tauri::AppHandle, holder: KeyboardHolder) {
    let _ = app.run_on_main_thread(move || {
        let slot = overlay_slot();
        let Some(overlay) = slot.as_ref() else { return };
        // SAFETY: main thread, and both views live for the window's lifetime.
        unsafe {
            let view = &*(overlay.view as *const OverlayView);
            let Some(window) = view.window() else { return };
            match holder {
                KeyboardHolder::Bar => {
                    if let Some(webview) = view.webview() {
                        let page = &*(webview as *const NSView);
                        window.makeFirstResponder(Some(page));
                    }
                }
                KeyboardHolder::Page => {
                    window.makeFirstResponder(Some(view as &NSResponder));
                }
            }
        }
    });
}

/// Give the keyboard back to the page: the bar's pane chord's ask.
pub fn focus_page(app: &tauri::AppHandle) {
    focus(app, KeyboardHolder::Page);
}

/// Bring the keyboard into the bar's document: the page asks when focus
/// lands on one of its controls while the picture still holds the keys.
pub fn focus_bar(app: &tauri::AppHandle) {
    focus(app, KeyboardHolder::Bar);
}

/// Close the surface: stop drawing and take the overlay down.
pub fn detach(app: &tauri::AppHandle) {
    let overlay = OVERLAY.lock().ok().and_then(|mut slot| slot.take());
    let Some(overlay) = overlay else { return };
    tauri_plugin_log::log::info!(
        "the picture is taken down (event_name client_browser_surface_detached)"
    );
    let view = overlay.view;
    let _ = app.run_on_main_thread(move || {
        // SAFETY: main thread, and the view is in the hierarchy.
        unsafe {
            let view = &*(view as *const OverlayView);
            if let Some(layer) = view.layer() {
                layer.setContents(None);
            }
            let window = view.window();
            let parent = view.superview();
            view.removeFromSuperview();
            // **And the keyboard goes back to the page.** The overlay held
            // first responder; without this the composer keeps none either.
            if let (Some(window), Some(parent)) = (window, parent) {
                for sibling in &parent.subviews() {
                    let pointer = (&*sibling as *const NSView) as usize;
                    if pointer != view as *const OverlayView as usize {
                        let _ = window.makeFirstResponder(Some(&*sibling));
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A picture that is not the box's shape is letterboxed, and a gesture
    /// maps through the rect it is drawn in.** Measured on a scratch host
    /// with the page's own mousedown log: a page laid out 1280x720 at 2x
    /// sends a 2560x1440 frame, drawn in a 1200x800 box as 1200x675 at y
    /// 62.5. A press on the picture at (900, 500) is the page's (960, 466.7);
    /// mapping the view's points 1:1 sent it to (900, 500), 60 px and 33 px
    /// from the pixel under the pointer.
    #[test]
    fn a_gesture_maps_through_the_rect_the_picture_is_drawn_in() {
        let bounds = (1200.0, 800.0);
        let picture = (2560.0, 1440.0);
        let page = (1280.0, 720.0);
        let (x, y) = point_in_page((900.0, 500.0), bounds, picture, page);
        assert!((x - 960.0).abs() < 0.01, "x is the page's 960, got {x}");
        assert!((y - 466.67).abs() < 0.01, "y is the page's 466.7, got {y}");
        let (x, y) = point_in_page((600.0, 62.5), bounds, picture, page);
        assert!((x - 640.0).abs() < 0.01, "the picture's middle is the page's 640, got {x}");
        assert!(y.abs() < 0.01, "and its top edge is the page's 0, got {y}");
    }

    /// **The mapping is built from the frame's own two sizes.** A page read
    /// from the pixels, or a page and a picture crossed, sends every
    /// letterboxed press somewhere else on the page, and nothing else in the
    /// draw path would notice.
    #[test]
    fn a_drawn_frame_names_both_sizes_the_mapping_reads() {
        let frame = Frame {
            profile: 1,
            tab: 1,
            width: 2560,
            height: 1440,
            page_width: 1280,
            page_height: 720,
            pixels: Vec::new(),
            alpha: true,
        };
        let picture = picture_of(&frame);
        assert_eq!(picture.pixels, (2560.0, 1440.0), "the picture is the frame's pixels");
        assert_eq!(picture.page, (1280.0, 720.0), "and the page is the viewport it came from");
    }

    /// **A picture taller than its box keeps its own aspect too**, so the map
    /// accounts for the left bar as well as the top one.
    #[test]
    fn a_picture_taller_than_its_box_maps_through_its_side_bars() {
        let bounds = (1200.0, 800.0);
        let picture = (1280.0, 1600.0);
        let page = (1280.0, 1600.0);
        let (x, y) = point_in_page((280.0, 0.0), bounds, picture, page);
        assert!(
            x.abs() < 0.01 && y.abs() < 0.01,
            "the picture's top-left is the page's, got {x},{y}"
        );
        let (x, y) = point_in_page((920.0, 800.0), bounds, picture, page);
        assert!(
            (x - 1280.0).abs() < 0.01 && (y - 1600.0).abs() < 0.01,
            "and its bottom-right is the page's corner, got {x},{y}"
        );
    }

    /// **A frame that already fills its box maps its points as they are.**
    #[test]
    fn a_picture_that_fills_its_box_maps_the_point_itself() {
        let bounds = (1200.0, 800.0);
        assert_eq!(
            point_in_page((400.0, 300.0), bounds, bounds, bounds),
            (400.0, 300.0),
            "no bars, no scaling"
        );
    }

    /// **A view with no picture drawn answers the page's origin** rather than
    /// dividing by an extent it does not have.
    #[test]
    fn a_view_with_no_extent_answers_the_page_origin() {
        assert_eq!(point_in_page((5.0, 5.0), (0.0, 0.0), (100.0, 50.0), (100.0, 50.0)), (0.0, 0.0));
        assert_eq!(point_in_page((5.0, 5.0), (100.0, 50.0), (0.0, 0.0), (100.0, 50.0)), (0.0, 0.0));
    }

    /// **The page measures top-down; a view that is not flipped measures
    /// from the bottom.** Skipping the mirror put the picture a bar's height
    /// high - over the bar and short at the bottom, measured on the dev
    /// stack - and the read-back is the same mirror.
    #[test]
    fn a_page_rect_is_mirrored_for_a_view_that_is_not_flipped() {
        assert_eq!(page_y(900.0, 40.0, 860.0, false), 0.0, "40 from the top is 0 from the bottom");
        assert_eq!(page_y(900.0, 40.0, 860.0, true), 40.0, "a flipped view already measures down");
        assert_eq!(
            page_y(900.0, 0.0, 860.0, false),
            40.0,
            "and the read-back mirrors the same way"
        );
    }

    /// **`flagsChanged:` is the only place a bare modifier exists.** The
    /// responder cannot be built headless - an `NSView` needs the main thread
    /// and a window - so what is pinned is the payload decision the method
    /// hands to `emit`: the side the person pressed and whether it is now
    /// down. The method itself is the ivars lock, the emit and this call.
    #[test]
    fn a_bare_modifier_carries_its_side_and_direction() {
        use objc2_app_kit::NSEventModifierFlags as Flags;
        let mut held = 0;
        assert_eq!(
            modifier_press(54, Flags::Command, &mut held),
            Some(ModifierPress { code: 54, down: true }),
            "the right Command going down"
        );
        assert_eq!(
            modifier_press(54, Flags::empty(), &mut held),
            Some(ModifierPress { code: 54, down: false }),
            "and coming up"
        );
        assert_eq!(
            modifier_press(59, Flags::Control, &mut held),
            Some(ModifierPress { code: 59, down: true }),
            "the left Control going down"
        );
        assert_eq!(
            modifier_press(62, Flags::empty(), &mut held),
            Some(ModifierPress { code: 62, down: false }),
            "and the right coming up"
        );
        assert_eq!(
            modifier_press(56, Flags::Shift, &mut held),
            None,
            "shift is not a side this client binds"
        );
        assert_eq!(modifier_press(0, Flags::empty(), &mut held), None, "nor any other key");
    }

    /// **The aggregate flag cannot tell the sides apart.** Holding one Command
    /// and pressing and releasing the other leaves `Command` set throughout;
    /// a direction read off the aggregate reported the release as a second
    /// press, so the page never ended the take its first press began.
    #[test]
    fn the_other_command_held_does_not_hide_a_release() {
        use objc2_app_kit::NSEventModifierFlags as Flags;
        let mut held = 0;
        assert_eq!(
            modifier_press(55, Flags::Command, &mut held).map(|press| press.down),
            Some(true),
            "the left Command goes down"
        );
        assert_eq!(
            modifier_press(54, Flags::Command, &mut held).map(|press| press.down),
            Some(true),
            "the right goes down while the left is held"
        );
        assert_eq!(
            modifier_press(54, Flags::Command, &mut held).map(|press| press.down),
            Some(false),
            "the right goes up while the left is still held"
        );
        assert_eq!(
            modifier_press(55, Flags::empty(), &mut held).map(|press| press.down),
            Some(false),
            "the left goes up"
        );
    }

    /// **The overlay's events cross a language boundary as literals.** A
    /// rename on either side alone kills desktop dictation, or leaves the
    /// pane chord blind to the keyboard, with every other test green, so the
    /// page's own listeners are read back out of the file.
    #[test]
    fn the_overlay_events_are_the_names_the_page_listens_for() {
        let page = include_str!("../../../../src/browser/shell.ts");
        for name in [MODIFIER_EVENT, MODIFIER_CHORD_EVENT, KEYBOARD_EVENT] {
            let listens = page.contains(&format!("listen('{name}'"))
                || page.contains(&format!("listen<unknown>('{name}'"));
            assert!(listens, "the page no longer listens for {name}");
        }
    }

    /// **The page moves the keyboard between the panes by a command name**,
    /// and the shell's handler list is where it has to exist. A rename on
    /// either side leaves the pane chord silently doing nothing.
    #[test]
    fn the_page_asks_for_the_page_keyboard_by_a_registered_command() {
        let page = include_str!("../../../../src/browser/shell.ts");
        let shell = include_str!("../../lib.rs");
        let command = "browser_surface_focus";
        assert!(
            page.contains(&format!("invokeShell('{command}'")),
            "the page no longer asks for {command}"
        );
        assert!(
            shell.contains(&format!("viewer::{command}")),
            "the shell's handler list lost {command}"
        );
    }

    /// **The pane chords are exactly Command+Option with an arrow**, so a
    /// page's Cmd+Option+letter, an arrow alone, or a third modifier stays
    /// the page's own keys.
    #[test]
    fn only_an_exact_command_option_arrow_moves_the_panes() {
        for keycode in [125, 126] {
            assert!(pane_key(keycode, true, false, true, false), "Cmd+Option and an arrow");
            assert!(
                !pane_key(keycode, true, false, true, true),
                "a Control in the chord makes it somebody else's"
            );
            assert!(!pane_key(keycode, true, true, true, false), "nor does Shift");
            assert!(!pane_key(keycode, true, false, false, false), "Command alone is the page's");
            assert!(!pane_key(keycode, false, false, true, false), "Option alone is too");
        }
        assert!(!pane_key(123, true, false, true, false), "the left arrow moves no panes");
        assert!(!pane_key(124, true, false, true, false), "nor the right");
    }

    /// **A chord is any key under a held Command or Control.** The page's
    /// press is discarded on its release; without this signal the release
    /// submits a take the person never meant to keep. A bare Shift is not a
    /// chord for this client, because no take starts on it.
    #[test]
    fn a_chord_is_a_key_under_a_held_command_or_control() {
        use objc2_app_kit::NSEventModifierFlags as Flags;
        assert!(chord_arrived(Flags::Command), "Command and a key");
        assert!(chord_arrived(Flags::Command | Flags::Shift), "Command and Shift and a key");
        assert!(chord_arrived(Flags::Control), "Control and a key");
        assert!(!chord_arrived(Flags::Shift), "Shift alone begins no take to discard");
        assert!(!chord_arrived(Flags::empty()), "and a plain key is not a chord");
    }

    /// **Only exactly Command is the takeover's**, and only with the overlay
    /// focused: everything else is the app's menu.
    #[test]
    fn only_an_exact_command_equivalent_is_the_surfaces() {
        assert_eq!(route_equivalent('w', true, false, false, false, true), Equivalent::Lower);
        assert_eq!(route_equivalent('[', true, false, false, false, true), Equivalent::Back);
        assert_eq!(route_equivalent(']', true, false, false, false, true), Equivalent::Forward);
        assert_eq!(route_equivalent('a', true, false, false, false, true), Equivalent::Page('a'));
        assert_eq!(route_equivalent('w', true, true, false, false, true), Equivalent::Defer);
        assert_eq!(route_equivalent('w', true, false, true, false, true), Equivalent::Defer);
        assert_eq!(route_equivalent('w', true, false, false, true, true), Equivalent::Defer);
        assert_eq!(route_equivalent('w', false, false, false, false, true), Equivalent::Defer);
        assert_eq!(
            route_equivalent('w', true, false, false, false, false),
            Equivalent::Defer,
            "the bar's feedback field keeps its own editing"
        );
    }
}
