//! The takeover's browser: the server's page, drawn in a native layer.
//!
//! **The browser is not this process's.** forge-server owns the host, the
//! driver and the profile; this half is a viewer - it attaches to a profile
//! over the socket, paints the frames the server relays, and sends the
//! person's gestures back. What the page under the overlay draws (the bar,
//! the hand-off's actions) is the webview's, exactly as before.
//!
//! **The overlay is the desktop's.** The phone draws the same frames in its
//! page instead (the canvas viewer), so nothing here runs on Android.

use serde::Deserialize;
// **Every macOS command here reads the viewer's state through `Manager`**,
// the one trait this file needs an `AppHandle` for.
#[cfg(target_os = "macos")]
use tauri::Manager as _;

/// Where the page says its takeover's page area is, in CSS pixels from the
/// viewport's top-left, and the device pixel ratio it is drawn at.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

impl Rect {
    #[cfg(target_os = "macos")]
    pub(crate) fn sane(self) -> Self {
        Self {
            x: self.x.max(0.0),
            y: self.y.max(0.0),
            width: self.width.max(1.0),
            height: self.height.max(1.0),
            scale: if self.scale > 0.0 { self.scale } else { 1.0 },
        }
    }
}

/// The page's own viewport, in CSS pixels.
///
/// **A titled window's page is shorter than the view the shell lays the
/// picture in**: 900 points of webview, 868 of page in a windowed run, the
/// difference being chrome above the page. The overlay is placed from the
/// page's coordinates, so the inset has to come with them or the top of the
/// picture covers the bar (full screen's inset is zero, which is why the
/// same code looked right there).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct Viewport {
    pub width: f64,
    pub height: f64,
}

impl Viewport {
    #[cfg(target_os = "macos")]
    pub(crate) fn sane(self) -> Self {
        Self { width: self.width.max(1.0), height: self.height.max(1.0) }
    }
}

#[cfg(target_os = "macos")]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod cef;
#[cfg(target_os = "macos")]
pub mod viewer;

#[cfg(target_os = "macos")]
pub use viewer::Viewer;

/// A client with no native overlay: the viewer commands exist so the page's
/// shared code can call them, and draw nothing. **The phone draws the frames
/// on a canvas in its page**, so it takes this arm too.
#[cfg(not(target_os = "macos"))]
pub mod viewer {
    use super::Rect;

    #[tauri::command]
    pub fn browser_attached(_id: u16) {}

    #[tauri::command]
    pub fn browser_surface_rect(_rect: Rect, _viewport: super::Viewport) {}
}

/// **A native resize moves the webview's frame, not the page's picture.**
///
/// The page re-measures into the new layout, but its own resize can land
/// before AppKit has settled the webview's frame - leaving the overlay laid
/// from the old geometry, which full screen's exit reproduces as a picture
/// over the bar. The remeasure is asked again once the window's resize has
/// settled, and the page's report then places the picture from the geometry
/// that is really there.
pub fn remeasure_soon(app: &tauri::AppHandle) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let mine = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(220)).await;
        if GENERATION.load(Ordering::Relaxed) != mine {
            return;
        }
        use tauri::Emitter as _;
        let _ = app.emit("browser_surface_remeasure", ());
    });
}

/// Show or lower the takeover's picture. **Lowering is a detach on both
/// platforms**: the server's idle clock counts an attached viewer as a use,
/// so a lowered surface gives its viewer up rather than keeping the profile
/// warm - and the phone attaches as a viewer too, even though its picture is
/// a canvas in the page rather than a native overlay.
///
/// **The takeover is the window's simple full screen.** Its bar is the
/// window's top edge and its page area the rest, so an open that left the
/// titled, non-full-screen window in place put the bar under the window's
/// own controls and the page area under its chrome. Simple full screen - the
/// door main used - keeps the window on the desktop with no title bar while
/// the takeover is up, and gives the window back on the lower.
#[tauri::command]
pub fn browser_takeover(app: tauri::AppHandle, on: bool) {
    // **The takeover is a layer in the window the person already has.** It
    // never enters full screen: the window keeps its title bar and its
    // traffic lights, so leaving the browser is the ordinary window, and
    // Back to chat only lowers this layer. **A full-screen takeover hid the
    // controls that get the person out** (Ved, 2026-10-24).
    #[cfg(target_os = "macos")]
    {
        if let Some(viewer) = app.try_state::<std::sync::Arc<Viewer>>() {
            viewer.show(on);
        }
    }
    if !on {
        crate::bridge::browser_detach(&app);
    }
}

/// One relayed frame, as both arms read it.
///
/// **A macOS build reads only the size and the pixels**: the sequence, the
/// format and the key flag are the phone arm's and the tests', so the struct
/// carries its fields for the arm that cannot be compiled here.
#[cfg_attr(
    target_os = "macos",
    allow(
        dead_code,
        reason = "the phone arm and the tests read these; the desktop overlay does not"
    )
)]
pub(crate) struct Relayed<'a> {
    pub profile: u16,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub format: u8,
    pub key: bool,
    pub encoded: &'a [u8],
}

/// One relayed frame's header, as both arms read it. `None` is a frame
/// shorter than its own header.
///
/// **One split for both arms**: the desktop's overlay and the phone's canvas
/// read the same host wire, so a drift in one cannot disagree with the other.
pub(crate) fn split_frame(bytes: &[u8]) -> Option<Relayed<'_>> {
    let (profile, rest) = bytes.split_first_chunk::<2>()?;
    let header = rest.get(..10)?;
    Some(Relayed {
        profile: u16::from_le_bytes(*profile),
        seq: u32::from_le_bytes([header[0], header[1], header[2], header[3]]),
        width: u16::from_le_bytes([header[4], header[5]]),
        height: u16::from_le_bytes([header[6], header[7]]),
        format: header[8],
        key: header[9] != 0,
        encoded: &rest[10..],
    })
}

/// One packet of the viewed page's sound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sound {
    pub profile: u16,
    pub seq: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Interleaved 16-bit samples.
    pub samples: Vec<i16>,
}

/// One packet's header: the profile, sequence, sample rate and channels;
/// the samples follow.
pub(crate) const SOUND_HEADER_LEN: usize = 11;

/// Split one audio packet's wire shape. `None` is a packet shorter than its
/// own header or one that does not hold whole samples.
pub(crate) fn split_sound(bytes: &[u8]) -> Option<Sound> {
    if bytes.len() < SOUND_HEADER_LEN {
        return None;
    }
    let profile = u16::from_le_bytes([bytes[0], bytes[1]]);
    let seq = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    let sample_rate = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    let channels = bytes[10];
    let rest = &bytes[SOUND_HEADER_LEN..];
    if !rest.len().is_multiple_of(2) || channels == 0 {
        return None;
    }
    let samples = rest.as_chunks::<2>().0.iter().map(|pair| i16::from_le_bytes(*pair)).collect();
    Some(Sound { profile, seq, sample_rate, channels, samples })
}

/// One packet of the page's sound, as the socket reader hands it over.
///
/// **The desktop hears it in the shell; the phone hears it in its page.** A
/// native player owns the desktop's device; on Android the packet becomes an
/// event and the page plays it with Web Audio, because a webpage is where
/// that device is reachable.
pub fn audio(app: &tauri::AppHandle, bytes: &[u8]) {
    let Some(sound) = split_sound(bytes) else {
        tauri_plugin_log::log::info!(
            "a relayed audio packet did not fit its header (event_name \
             client_browser_audio_undecodable, {} bytes)",
            bytes.len()
        );
        return;
    };
    #[cfg(target_os = "macos")]
    {
        // **The device is AppKit's**: the engine is built and fed on the
        // main thread, whatever thread the socket reader is on.
        let sound = sound.clone();
        let _ = app.run_on_main_thread(move || crate::browser::audio::play(&sound));
    }
    #[cfg(not(target_os = "macos"))]
    {
        use base64::Engine as _;
        use tauri::Emitter as _;
        let mut pcm = Vec::with_capacity(sound.samples.len() * 2);
        for sample in &sound.samples {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let _ = app.emit(
            "client://browser-audio",
            serde_json::json!({
                "profile": sound.profile,
                "seq": sound.seq,
                "rate": sound.sample_rate,
                "channels": sound.channels,
                "samples": base64::engine::general_purpose::STANDARD.encode(&pcm),
            }),
        );
    }
}

/// One relayed browser frame, as the socket reader hands it over: the
/// viewer draws it if it belongs to the surface this page has open.
///
/// **A frame with no viewer is dropped**: a client that is not showing a
/// takeover is not this frame's to draw. The phone hands the frame to its
/// page instead, which paints it on a canvas (see `viewer` above).
pub fn frame(app: &tauri::AppHandle, bytes: &[u8]) {
    #[cfg(target_os = "macos")]
    {
        if let Some(viewer) = app.try_state::<std::sync::Arc<Viewer>>() {
            viewer.on_frame(bytes);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        use base64::Engine as _;
        use tauri::Emitter as _;

        let Some(frame) = split_frame(bytes) else {
            // **A frame this half cannot read is said**, not dropped quietly:
            // the page's frames come through here and a missing one has no
            // other trace. **Info, not debug**: the client logger keeps
            // `Info`, so a debug line would never reach `forge.log`.
            tauri_plugin_log::log::info!(
                "a relayed frame did not fit its header (event_name \
                 client_browser_frame_undecodable, {} bytes)",
                bytes.len()
            );
            return;
        };
        let body = serde_json::json!({
            "profile": frame.profile,
            "seq": frame.seq,
            "width": frame.width,
            "height": frame.height,
            "format": frame.format,
            "key": frame.key,
            "bytes": base64::engine::general_purpose::STANDARD.encode(frame.encoded),
        });
        // **The newest frame is KEPT for the page that arrives late.** An
        // attach's key frame can be emitted before the surface mounts and
        // subscribes, and a still page never repaints: the page asks for
        // this on mount rather than staying black.
        *KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(body.clone());
        let _ = app.emit("client://browser-frame", body);
    }
}

/// The newest relayed frame, as the phone's canvas paints it on mount.
///
/// **The desktop's overlay answers `null`**: its viewer keeps the frame in
/// its own state and draws it when the surface is laid.
#[tauri::command]
pub fn browser_kept_frame() -> Option<serde_json::Value> {
    #[cfg(not(target_os = "macos"))]
    {
        KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
    #[cfg(target_os = "macos")]
    {
        None
    }
}

#[cfg(not(target_os = "macos"))]
static KEPT: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);

/// Whether the kept phone frame belongs to the profile this attach names.
///
/// **A new attach drops another profile's frame**: the canvas mounts before
/// the new `browser_view` lands, so without this it could paint the previous
/// profile's picture under this one's name.
#[cfg(any(not(target_os = "macos"), test))]
fn keeps_kept(kept: Option<&serde_json::Value>, id: u16) -> bool {
    kept.and_then(|frame| frame.get("profile")).and_then(serde_json::Value::as_u64)
        == Some(u64::from(id))
}

/// The server named the viewed profile's wire id.
pub fn attached(app: &tauri::AppHandle, id: u16) {
    #[cfg(target_os = "macos")]
    {
        if let Some(viewer) = app.try_state::<std::sync::Arc<Viewer>>() {
            viewer.attached(id);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        // **A new attach drops the previous profile's kept frame.** The
        // canvas mounts before the new `browser_view` lands, so without this
        // it could paint the last profile's picture under this one's name.
        let mut kept = KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !keeps_kept(kept.as_ref(), id) {
            *kept = None;
        }
    }
}

/// The viewed profile's browser stopped: the overlay goes with it. **The
/// window is left alone** - the takeover never took its title bar, so there
/// is no full screen to give back, and a person's own full screen must not be
/// ended by a browser dying.
pub fn lost(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    {
        if let Some(viewer) = app.try_state::<std::sync::Arc<Viewer>>() {
            viewer.lost();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
}

#[cfg(test)]
mod tests {
    use super::{keeps_kept, split_frame};

    /// **The kept phone frame stays only for its own profile.** A canvas
    /// mounting before the new `browser_view` would otherwise paint the
    /// previous profile's picture under this one's name.
    #[test]
    fn a_kept_frame_stays_only_for_its_profile() {
        let frame = serde_json::json!({ "profile": 5 });
        assert!(keeps_kept(Some(&frame), 5), "the same profile keeps its frame");
        assert!(!keeps_kept(Some(&frame), 6), "another profile drops it");
        assert!(!keeps_kept(None, 5), "nothing kept is not kept for anyone");
        assert!(
            !keeps_kept(Some(&serde_json::json!({ "profile": "5" })), 5),
            "a value this half cannot read is not a match",
        );
    }

    /// **A relayed frame splits into the fields both arms draw by.** The
    /// desktop's overlay reads the size and the pixels, the phone's canvas
    /// the profile and the sequence; a header parsed at the wrong offset
    /// draws shifted rows or another profile's page.
    #[test]
    fn a_relayed_frame_splits_its_header() {
        let mut bytes = vec![7_u8, 0]; // profile 7
        bytes.extend_from_slice(&9_u32.to_le_bytes()); // seq 9
        bytes.extend_from_slice(&640_u16.to_le_bytes());
        bytes.extend_from_slice(&480_u16.to_le_bytes());
        bytes.push(1); // WebP
        bytes.push(1); // a key frame
        bytes.extend_from_slice(&[4, 3, 2, 1]);

        let split = split_frame(&bytes).expect("a whole frame");
        assert_eq!(
            (
                split.profile,
                split.seq,
                split.width,
                split.height,
                split.format,
                split.key,
                split.encoded,
            ),
            (7, 9, 640, 480, 1, true, &[4_u8, 3, 2, 1][..]),
            "the profile, sequence, size, format, key flag and payload"
        );
        assert!(split_frame(&bytes[..11]).is_none(), "a frame shorter than its header is refused");
    }
}
