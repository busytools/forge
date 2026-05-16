//! Image-attachment types + validation. Lifted from
//! `forge-tui::app::clipboard_image` so forge-agent and forge-tui can
//! share the wire-shape type without forge-agent reaching into the UI
//! crate.
//!
//! Encoding-from-clipboard helpers stay in forge-tui (they pull
//! `arboard` + `image` crates which only the UI cares about).

use serde::{Deserialize, Serialize};

/// MIME types the Anthropic Vision API accepts as image attachments.
pub const SUPPORTED_IMAGE_MIME_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// A pending image attachment: base64-encoded data and its MIME type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub data: String,
    pub mime_type: String,
}

/// Returns `true` if `mime_type` is a supported image MIME type.
pub fn is_supported_image_type(mime_type: &str) -> bool {
    SUPPORTED_IMAGE_MIME_TYPES.contains(&mime_type)
}

/// Returns `true` if `data` is non-empty, correctly padded, and decodes
/// as valid standard base64.
pub fn is_valid_base64(data: &str) -> bool {
    use base64::Engine as _;
    if data.is_empty() {
        return false;
    }
    let clean = data.trim();
    if !clean.len().is_multiple_of(4) {
        return false;
    }
    base64::engine::general_purpose::STANDARD.decode(clean).is_ok()
}

/// Validate an image attachment before sending to the API.
///
/// # Errors
///
/// Returns a human-readable description of the first failure: unsupported
/// MIME type or invalid base64.
pub fn validate_image(data: &str, mime_type: &str) -> Result<(), String> {
    if !is_supported_image_type(mime_type) {
        return Err(format!(
            "unsupported image type \"{mime_type}\"; expected one of: {}",
            SUPPORTED_IMAGE_MIME_TYPES.join(", ")
        ));
    }
    if !is_valid_base64(data) {
        return Err("image data is not valid base64".to_owned());
    }
    Ok(())
}
