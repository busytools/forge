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

/// A browser hand-off: a session asking the person at a client to act in a
/// browser tab before it can carry on.
///
/// The parked shape crosses the socket as `BrowserHandOffPending` and the
/// dock draws it; `id` is what the answer comes back under, the same id the
/// registry is keyed by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandOff {
    pub id: uuid::Uuid,
    /// What the session needs done, in its own words.
    pub reason: String,
    /// The named profile to raise, when the session named one.
    pub profile: Option<String>,
}

/// Why a hand-off left the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandOffEnding {
    /// The person acted; the session carries on.
    Done,
    /// The person declined; the session knows what to do instead.
    NotNow,
    /// The asking session went away before anyone answered.
    Abandoned,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-off crosses as its own words plus the profile it named - and
    /// one that named no profile carries none, rather than an empty string a
    /// dock would have to read as "the default".
    #[test]
    fn a_hand_off_crosses_with_what_it_asked_for() {
        let handoff = HandOff {
            id: uuid::Uuid::from_u128(7),
            reason: "the sign-in page wants a CAPTCHA".to_owned(),
            profile: Some("job-hunt".to_owned()),
        };
        let encoded = serde_json::to_value(&handoff).expect("a hand-off encodes");
        assert_eq!(
            encoded,
            serde_json::json!({
                "id": "00000000-0000-0000-0000-000000000007",
                "reason": "the sign-in page wants a CAPTCHA",
                "profile": "job-hunt",
            }),
        );
        assert_eq!(
            serde_json::from_value::<HandOff>(encoded).expect("a hand-off decodes"),
            handoff,
        );

        let bare = HandOff {
            id: uuid::Uuid::from_u128(8),
            reason: "look at it".to_owned(),
            profile: None,
        };
        let encoded = serde_json::to_value(&bare).expect("a bare hand-off encodes");
        assert_eq!(
            serde_json::from_value::<HandOff>(encoded).expect("a bare hand-off decodes"),
            bare,
            "a hand-off that named no profile decodes as one that named none",
        );
    }

    /// An ending crosses as its tag: the resolved update carries it, and the
    /// view that did not answer says what happened with the same word the
    /// answer had.
    #[test]
    fn an_ending_crosses_as_its_tag() {
        assert_eq!(
            serde_json::to_value(HandOffEnding::Done).expect("an ending encodes"),
            serde_json::json!({ "type": "done" }),
        );
        assert_eq!(
            serde_json::from_value::<HandOffEnding>(serde_json::json!({ "type": "not_now" }))
                .expect("an ending decodes"),
            HandOffEnding::NotNow,
        );
        assert_eq!(
            serde_json::to_value(HandOffEnding::Abandoned).expect("an ending encodes"),
            serde_json::json!({ "type": "abandoned" }),
        );
    }

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
