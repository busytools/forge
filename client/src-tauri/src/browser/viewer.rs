//! The desktop viewer: the state a surface holds, and the frame route that
//! draws it.
//!
//! One surface at a time, and the server's browser outlives it: lowering the
//! takeover detaches the picture, not the page. A frame for a tab this page
//! is not looking at is dropped - the server relays only what this
//! connection attached to, so a mismatch is a frame from before an attach.

#![allow(
    clippy::needless_pass_by_value,
    reason = "tauri's IPC macro hands every command argument by value; a reference is not a \
              CommandArg"
)]

use std::sync::{Arc, Mutex};

use super::cef::surface::KeyboardHolder;
use super::{Rect, Viewport};

/// The pane a holder string names, or `None` for one this shell does not
/// have. **A swapped pair here moves the keyboard to the wrong pane with
/// every other test green**, which is why it is its own function.
fn pane_of(holder: &str) -> Option<KeyboardHolder> {
    match holder {
        "bar" => Some(KeyboardHolder::Bar),
        "page" => Some(KeyboardHolder::Page),
        _ => None,
    }
}

/// What the viewer knows about the surface the person has open.
struct State {
    /// The profile id the page attached to, from the server's `browser_view`.
    id: Option<u16>,
    /// The tab the page attached to: the other half of the frame filter.
    tab: Option<u16>,
    /// The page area, as the page last reported it.
    rect: Option<Rect>,
    /// The page's own viewport, reported beside the area: the inset the
    /// window's chrome takes from the webview the picture is laid in.
    viewport: Option<Viewport>,
    /// Whether the takeover is up: the overlay is laid while it is.
    shown: bool,
    /// The newest frame, kept so a re-lay (a resize, a re-show) can draw
    /// before the next one arrives.
    frame: Option<Arc<super::cef::surface::Frame>>,
}

/// The desktop's browser viewer, managed by the app.
pub struct Viewer {
    state: Mutex<State>,
    app: tauri::AppHandle,
}

impl Viewer {
    pub fn new(app: tauri::AppHandle) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                id: None,
                tab: None,
                rect: None,
                viewport: None,
                shown: false,
                frame: None,
            }),
            app,
        })
    }

    /// The server named the tab this page is viewing: frames for that
    /// `(profile, tab)` are this surface's, and any other's are not. A new
    /// tab drops the old picture rather than showing another page.
    pub fn attached(&self, id: u16, tab: u16) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.id != Some(id) || state.tab != Some(tab) {
            state.id = Some(id);
            state.tab = Some(tab);
            state.frame = None;
        }
    }

    /// Whether a sound packet belongs to the tab this viewer shows. A packet
    /// may beat the attach's own frame, as a frame does, so the first one is
    /// adopted rather than dropped into silence.
    pub fn holds(&self, profile: u16, tab: u16) -> bool {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match belongs(state.id, state.tab, profile, tab) {
            Belonging::Adopt => {
                state.id = Some(profile);
                state.tab = Some(tab);
                true
            }
            Belonging::Mine => true,
            Belonging::Other => false,
        }
    }

    /// The viewed profile's browser stopped: the picture goes with it, and
    /// the page draws its own failure sentence.
    pub fn lost(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.shown = false;
        state.id = None;
        state.tab = None;
        state.frame = None;
        super::cef::surface::detach(&self.app);
    }

    /// The page reports its page area and viewport: lay the overlay there,
    /// or move it.
    pub fn set_rect(&self, rect: Rect, viewport: Viewport) {
        let rect = rect.sane();
        let viewport = viewport.sane();
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.rect = Some(rect);
        state.viewport = Some(viewport);
        if state.shown {
            super::cef::surface::attach(&self.app, rect, viewport);
            if let Some(frame) = &state.frame {
                super::cef::surface::draw(&self.app, frame);
            }
        }
    }

    /// Show or lower the takeover's picture.
    ///
    /// **A show may land before the page reports its page area**: the rect's
    /// own report lays the overlay then, so an open is not a race with the
    /// page's first layout.
    pub fn show(&self, on: bool) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.shown = on;
        if on {
            if let (Some(rect), Some(viewport)) = (state.rect, state.viewport) {
                super::cef::surface::attach(&self.app, rect, viewport);
                if let Some(frame) = &state.frame {
                    super::cef::surface::draw(&self.app, frame);
                }
            }
        } else {
            super::cef::surface::detach(&self.app);
        }
    }

    /// **The keyboard goes to the page or to the bar's document**, whichever
    /// the page asked for: the overlay's view holds it for the picture, and
    /// the webview it was attached with holds the bar's.
    pub fn focus(&self, holder: &str) {
        match pane_of(holder) {
            Some(KeyboardHolder::Bar) => super::cef::surface::focus_bar(&self.app),
            Some(KeyboardHolder::Page) => super::cef::surface::focus_page(&self.app),
            None => tauri_plugin_log::log::warn!(
                "the page asked for a keyboard holder this shell does not have ({holder}) \
                 (event_name client_keyboard_unknown_holder)"
            ),
        }
    }

    /// One relayed frame, as the socket carried it: the host's own frame
    /// wire - profile id, tab, sequence, view size, format, key flag and the
    /// page's viewport, then the encoded pixels.
    pub fn on_frame(&self, bytes: &[u8]) {
        let Some(frame) = super::split_frame(bytes) else {
            // **A frame this viewer cannot read is said**, not dropped with
            // no trace: the socket hands over every binary frame it takes,
            // and a missing one is otherwise a dark page with no line.
            short_frame(bytes.len());
            return;
        };
        let (profile, tab, width, height) = (frame.profile, frame.tab, frame.width, frame.height);
        let page = (u32::from(frame.page_width), u32::from(frame.page_height));
        let payload = frame.encoded;
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match belongs(state.id, state.tab, profile, tab) {
            // **The first picture may beat the attach's own frame.** The
            // text frame naming the tab and the binary frames race on the
            // socket, and the server relays only the tab this connection
            // attached to - so the first frame IS that tab's, and dropping
            // it left a still page dark for ever.
            Belonging::Adopt => {
                state.id = Some(profile);
                state.tab = Some(tab);
            }
            Belonging::Mine => {}
            Belonging::Other => {
                tauri_plugin_log::log::debug!(
                    "a frame for another tab was dropped (event_name \
                     client_browser_frame_other_tab, profile {profile}, tab {tab}, viewing \
                     {:?}/{:?})",
                    state.id,
                    state.tab
                );
                return;
            }
        }
        let Some(frame) = super::cef::surface::decode(
            profile,
            tab,
            payload,
            u32::from(width),
            u32::from(height),
            page,
        ) else {
            undecodable_frame(profile, tab, width, height, payload.len());
            return;
        };
        let frame = Arc::new(frame);
        state.frame = Some(Arc::clone(&frame));
        if state.shown {
            super::cef::surface::draw(&self.app, &frame);
        }
    }
}

/// Say a relayed frame that did not fit its own header.
///
/// **Info, not debug**: this module is not one of the default directives'
/// named targets, so a debug line here would never reach `forge.log`.
fn short_frame(bytes: usize) {
    tauri_plugin_log::log::info!(
        "a relayed frame did not fit its header (event_name client_browser_frame_undecodable, \
         {bytes} bytes)"
    );
}

/// What a relayed frame's ids say about the tab this viewer attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Belonging {
    /// No attach has named a tab yet, so this frame is taken as its own.
    Adopt,
    /// The tab this viewer is showing.
    Mine,
    /// Another tab's page.
    Other,
}

/// Whether a frame is the attached tab's, another's, or the first one.
fn belongs(id: Option<u16>, tab: Option<u16>, profile: u16, frame_tab: u16) -> Belonging {
    match (id, tab) {
        (Some(id), Some(tab)) if id == profile && tab == frame_tab => Belonging::Mine,
        (Some(_), Some(_)) => Belonging::Other,
        _ => Belonging::Adopt,
    }
}

/// Say a frame whose pixels did not decode, naming the tab it belonged to.
fn undecodable_frame(profile: u16, tab: u16, width: u16, height: u16, bytes: usize) {
    tauri_plugin_log::log::info!(
        "a relayed frame did not decode (event_name client_browser_frame_undecodable, \
         profile {profile}, tab {tab}, {width}x{height}, {bytes} bytes)"
    );
}

/// The page's page area, as the takeover's layout reports it.
#[tauri::command]
pub fn browser_surface_rect(viewer: tauri::State<'_, Arc<Viewer>>, rect: Rect, viewport: Viewport) {
    viewer.set_rect(rect, viewport);
}

/// Hand the keyboard to the pane the page names: `bar` or `page`.
#[tauri::command]
pub fn browser_surface_focus(viewer: tauri::State<'_, Arc<Viewer>>, holder: String) {
    viewer.focus(&holder);
}

#[cfg(test)]
mod tests {
    use super::{Belonging, KeyboardHolder, belongs, pane_of};

    /// **The two holder strings name the panes they say.** The page sends
    /// `bar` for the webview and `page` for the picture, and a swapped pair
    /// would move the keyboard to the wrong one with every other test green.
    #[test]
    fn the_holder_strings_name_the_panes_they_say() {
        assert_eq!(pane_of("bar"), Some(KeyboardHolder::Bar), "bar is the webview");
        assert_eq!(pane_of("page"), Some(KeyboardHolder::Page), "page is the picture");
        assert_eq!(pane_of("Bar"), None, "the wire is lower case, and a typo moves nothing");
        assert_eq!(pane_of(""), None);
    }

    /// **A frame for another tab never paints, and the first picture is
    /// adopted.** The desktop overlay draws one tab at a time, so a frame
    /// that names another tab is another page's picture; the first frame
    /// may beat the attach's own frame, and dropping it left a still page
    /// dark for ever.
    #[test]
    fn a_frame_belongs_to_the_tab_that_attached() {
        assert_eq!(belongs(Some(5), Some(2), 5, 2), Belonging::Mine, "the viewed tab's frame");
        assert_eq!(belongs(Some(5), Some(2), 5, 3), Belonging::Other, "another tab's frame");
        assert_eq!(belongs(Some(5), Some(2), 6, 2), Belonging::Other, "another profile's frame");
        assert_eq!(belongs(None, None, 5, 2), Belonging::Adopt, "the first frame is the tab's");
        assert_eq!(belongs(Some(5), None, 5, 2), Belonging::Adopt, "a half-named viewer adopts");
    }

    /// **A frame the desktop viewer cannot read is said at a level the
    /// client keeps** - a short header and a decode failure alike. Both were
    /// at `debug`, which the directives' `Info` baseline never writes to
    /// `forge.log`.
    #[test]
    fn an_unreadable_relayed_frame_is_said_at_info() {
        let _held = crate::log_capture::capture();
        super::short_frame(7);
        super::undecodable_frame(3, 4, 800, 600, 42);
        let lines = crate::log_capture::lines();
        assert!(
            lines.iter().any(|line| {
                line.contains("client_browser_frame_undecodable") && line.contains("7 bytes")
            }),
            "the short header is named: {lines:?}",
        );
        assert!(
            lines.iter().any(|line| {
                line.contains("client_browser_frame_undecodable")
                    && line.contains("profile 3")
                    && line.contains("tab 4")
                    && line.contains("800x600")
                    && line.contains("42 bytes")
            }),
            "the decode failure names the profile and its size: {lines:?}",
        );
    }
}
