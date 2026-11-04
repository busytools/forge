//! The terminal's bytes on the shell's socket.
//!
//! **The page does not hold the socket on the desktop**, so a terminal's
//! output has to reach the webview. There is no native overlay for a
//! terminal, so the page draws it with xterm.js, and the path is the same one
//! the browser takes on every target but macOS: a kind-4 frame becomes an
//! event carrying the terminal id, the sequence and the bytes base64, and the
//! page decodes them into xterm.
//!
//! The batching that keeps 60 fps of small writes from becoming 60 IPC calls
//! is the page's, where the animation frame is.

use base64::Engine as _;
use tauri::Emitter as _;

/// One relayed terminal output frame, as the socket reader hands it over: a
/// little-endian id and sequence, then the shell's bytes. **The kind tag is
/// already stripped by the socket's dispatch**, which is what tells this
/// sink apart from the browser's.
pub fn frame(app: &tauri::AppHandle, bytes: &[u8]) {
    if bytes.len() < 6 {
        // **A frame this half cannot read is said**, not dropped quietly: the
        // page's stream has no other trace of a missing one.
        tauri_plugin_log::log::info!(
            "a relayed terminal frame did not fit its header (event_name \
             client_terminal_frame_undecodable, {} bytes)",
            bytes.len()
        );
        return;
    }
    let id = u16::from_le_bytes([bytes[0], bytes[1]]);
    let seq = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    let _ = app.emit(
        "client://terminal-frame",
        serde_json::json!({
            "id": id,
            "seq": seq,
            "bytes": base64::engine::general_purpose::STANDARD.encode(&bytes[6..]),
        }),
    );
}
