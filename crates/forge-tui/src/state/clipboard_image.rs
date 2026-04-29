#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    reason = "lifted upstream from claude-code-rust (string-parsing subset only)"
)]

//! Subset of upstream `app/clipboard_image.rs` that input/UI need: image
//! badge span parsing, attachment struct, MIME-type whitelist. The
//! clipboard-reading + base64 encoding bits require `arboard` + `image` +
//! `base64` and stay deferred until Ctrl+V image attach is wired.

pub const SUPPORTED_IMAGE_MIME_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub data: String,
    pub mime_type: String,
}

#[must_use]
pub fn is_supported_image_type(mime_type: &str) -> bool {
    SUPPORTED_IMAGE_MIME_TYPES.contains(&mime_type)
}

/// Find `[Image #N]` badge spans in a line.
///
/// Returns `(byte_start, byte_end, 1-based_index)` for each badge found.
#[must_use]
pub fn find_image_badge_spans(line: &str) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    let mut search_from = 0;
    while let Some(start) = line[search_from..].find("[Image #") {
        let abs_start = search_from + start;
        if let Some(end_rel) = line[abs_start..].find(']') {
            let abs_end = abs_start + end_rel + 1;
            let inner = &line[abs_start + 8..abs_start + end_rel];
            if !inner.is_empty()
                && inner.chars().all(|c| c.is_ascii_digit())
                && let Ok(idx) = inner.parse::<usize>()
            {
                spans.push((abs_start, abs_end, idx));
            }
            search_from = abs_end;
        } else {
            break;
        }
    }
    spans
}
