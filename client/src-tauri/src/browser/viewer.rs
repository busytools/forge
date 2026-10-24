//! The desktop viewer: the state a surface holds, and the frame route that
//! draws it.
//!
//! One surface at a time, and the server's browser outlives it: lowering the
//! takeover detaches the picture, not the page. A frame for a profile this
//! page is not looking at is dropped - the server relays only what this
//! connection attached to, so a mismatch is a frame from before an attach.

use std::sync::{Arc, Mutex};

use super::{Rect, Viewport};

/// What the viewer knows about the surface the person has open.
struct State {
    /// The profile id the page attached to, from the server's `browser_view`.
    id: Option<u16>,
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
                rect: None,
                viewport: None,
                shown: false,
                frame: None,
            }),
            app,
        })
    }

    /// The server named the profile this page is viewing: frames for that id
    /// are this surface's, and any other profile's are not. A new profile's
    /// id drops the old picture rather than showing another browser's page.
    pub fn attached(&self, id: u16) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.id != Some(id) {
            state.id = Some(id);
            state.frame = None;
        }
    }

    /// The viewed profile's browser stopped: the picture goes with it, and
    /// the page draws its own failure sentence.
    pub fn lost(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.shown = false;
        state.id = None;
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

    /// One relayed frame, as the socket carried it: the host's own frame
    /// wire - profile id, sequence, view size, format and key flag, then the
    /// encoded pixels.
    pub fn on_frame(&self, bytes: &[u8]) {
        let Some(frame) = super::split_frame(bytes) else {
            // **A frame this viewer cannot read is said**, not dropped with
            // no trace: the socket hands over every binary frame it takes,
            // and a missing one is otherwise a dark page with no line.
            short_frame(bytes.len());
            return;
        };
        let (profile, width, height) = (frame.profile, frame.width, frame.height);
        let payload = frame.encoded;
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // **The first picture may beat the attach's own frame.** The text
        // frame naming the profile and the binary frames race on the socket,
        // and the server relays only the profile this connection attached
        // to - so the first frame IS that profile, and dropping it left a
        // still page dark for ever.
        if state.id.is_none() {
            state.id = Some(profile);
        }
        if state.id != Some(profile) {
            tauri_plugin_log::log::debug!(
                "a frame for another profile was dropped (event_name \
                 client_browser_frame_other_profile, profile {profile}, viewing {:?})",
                state.id
            );
            return;
        }
        let Some(frame) =
            super::cef::surface::decode(profile, payload, u32::from(width), u32::from(height))
        else {
            undecodable_frame(profile, width, height, payload.len());
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
/// **Info, not debug**: the client logger keeps `Info`, so a debug line here
/// would never reach `forge.log`.
fn short_frame(bytes: usize) {
    tauri_plugin_log::log::info!(
        "a relayed frame did not fit its header (event_name client_browser_frame_undecodable, \
         {bytes} bytes)"
    );
}

/// Say a frame whose pixels did not decode, naming the profile it belonged to.
fn undecodable_frame(profile: u16, width: u16, height: u16, bytes: usize) {
    tauri_plugin_log::log::info!(
        "a relayed frame did not decode (event_name client_browser_frame_undecodable, \
         profile {profile}, {width}x{height}, {bytes} bytes)"
    );
}

/// The server named the profile the page is viewing.
#[tauri::command]
pub fn browser_attached(viewer: tauri::State<'_, Arc<Viewer>>, id: u16) {
    viewer.attached(id);
}

/// The page's page area, as the takeover's layout reports it.
#[tauri::command]
pub fn browser_surface_rect(viewer: tauri::State<'_, Arc<Viewer>>, rect: Rect, viewport: Viewport) {
    viewer.set_rect(rect, viewport);
}

#[cfg(test)]
mod tests {
    /// **A frame the desktop viewer cannot read is said at a level the
    /// client keeps** - a short header and a decode failure alike. Both were
    /// at `debug`, which the `Info` filter never writes to `forge.log`.
    #[test]
    fn an_unreadable_relayed_frame_is_said_at_info() {
        let _held = crate::log_capture::capture();
        super::short_frame(7);
        super::undecodable_frame(3, 800, 600, 42);
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
                    && line.contains("800x600")
                    && line.contains("42 bytes")
            }),
            "the decode failure names the profile and its size: {lines:?}",
        );
    }
}
