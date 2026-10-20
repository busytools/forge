//! Image-attachment wire-shape types + validation, shared between
//! forge-agent and a view.

use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::messages::Extras;

/// MIME types the Anthropic Vision API accepts as image attachments.
pub const SUPPORTED_IMAGE_MIME_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// A pending image attachment: base64-encoded data and its MIME type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub data: String,
    pub mime_type: String,
}

impl ImageAttachment {
    /// The content block a view draws: the mime the row names and the size it
    /// prints, never the payload - a frame carrying the base64 is re-served by
    /// every read of the row.
    pub fn display_block(&self) -> ContentBlock {
        let mut extras = Extras::new();
        extras.insert("forge_bytes".to_owned(), serde_json::json!(payload_bytes(&self.data)));
        ContentBlock::Image {
            source: serde_json::json!({
                "type": "base64",
                "media_type": self.mime_type,
            }),
            extras,
        }
    }
}

/// The bytes a base64 payload stands for: four characters carry three, and
/// padding is not a byte.
pub fn payload_bytes(data: &str) -> u64 {
    let padding = if data.ends_with("==") { 2 } else { u64::from(data.ends_with('=')) };
    (data.len() as u64 * 3 / 4).saturating_sub(padding)
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
