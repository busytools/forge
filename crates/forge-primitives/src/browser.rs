//! What a browser tool call hands back from the client that drove it.
//!
//! The socket's ask/answer pair carries these: an answer is the parts a tool
//! returns, in order, and one of them may be an image. **The image's bytes are
//! not in this type's wire form.** They ride their own binary frame under the
//! answer's id (`forge-server`'s `transport::frame`), because a screenshot is
//! megabytes and base64 in JSON pays a third again for it; the mime type
//! crosses here so the frame carries nothing but the bytes.

use serde::{Deserialize, Serialize};

/// One part of a browser tool's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserPart {
    /// Text the tool returns as its content.
    Text { text: String },
    /// An image, whose bytes follow in their own binary frame under the same
    /// answer id. Empty until the frame arrives.
    Image {
        mime_type: String,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes are the frame's, not this type's: a wire form that carried
    /// them would be the base64 balloon the frame exists to avoid.
    #[test]
    fn an_image_part_crosses_with_its_mime_and_not_its_bytes() {
        let part = BrowserPart::Image { mime_type: "image/png".to_owned(), bytes: vec![1, 2, 3] };
        let encoded = serde_json::to_value(&part).expect("a part encodes");
        assert_eq!(
            encoded,
            serde_json::json!({ "type": "image", "mime_type": "image/png" }),
            "the wire form carries the tag and the mime, and the bytes ride the frame",
        );
        let BrowserPart::Image { mime_type, bytes } =
            serde_json::from_value(encoded).expect("a part decodes")
        else {
            panic!("an image part decodes into another part")
        };
        assert_eq!(mime_type, "image/png");
        assert!(bytes.is_empty(), "and it decodes with no bytes until the frame fills them");
    }

    #[test]
    fn a_text_part_crosses_as_its_text() {
        let encoded = serde_json::to_value(BrowserPart::Text { text: "done".to_owned() })
            .expect("a part encodes");
        assert_eq!(encoded, serde_json::json!({ "type": "text", "text": "done" }));
    }
}
