//! What a browser tool call hands back from the server's browser.
//!
//! A call answers the parts the tool returned, in order: text, or an image
//! whose bytes the MCP host hands to the CLI as their own content block. **The
//! browser is the server's**, so none of this crosses the socket; and the
//! image's bytes are not in this type's JSON form either, because a screenshot
//! is megabytes and base64 in JSON would pay a third again for it.

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

/// One org's browser profile, `<org>/<name>`: two path-safe segments, and the
/// unit every profile lookup is keyed by.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProfileKey(String);

impl ProfileKey {
    pub fn of(org: &str, name: &str) -> Result<Self, String> {
        Self::parse(&format!("{org}/{name}"))
    }

    /// Parse `<org>/<name>`; both halves become directory names.
    pub fn parse(key: &str) -> Result<Self, String> {
        let Some((org, name)) = key.split_once('/') else {
            return Err(format!("'{key}' is not a browser profile key: the shape is <org>/<name>"));
        };
        segment_refusal(org, "org")?;
        segment_refusal(name, "name")?;
        Ok(Self(key.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn org(&self) -> &str {
        self.0.split_once('/').map(|(org, _)| org).unwrap_or_default()
    }

    pub fn name(&self) -> &str {
        self.0.split_once('/').map(|(_, name)| name).unwrap_or_default()
    }

    /// A fixture's key, from parts the fixture chose and so already knows
    /// are safe.
    #[cfg(feature = "test-helpers")]
    pub fn from_parts_for_test(org: &str, name: &str) -> Self {
        Self(format!("{org}/{name}"))
    }
}

/// One segment of a profile key: what can be a directory name and nothing
/// else. A name is the profile charset; an org may carry any character a
/// directory holds, but never a separator or a dot traversal.
fn segment_refusal(segment: &str, what: &str) -> Result<(), String> {
    let refused = segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.contains(['/', '\\', '\0']);
    if refused {
        return Err(format!("'{segment}' cannot be a browser profile {what}"));
    }
    if what == "name" {
        let allowed = segment.len() <= 64
            && segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !allowed {
            return Err(format!(
                "'{segment}' cannot be a browser profile name: 1 to 64 characters of letters, \
                 digits, dot, dash or underscore"
            ));
        }
    }
    Ok(())
}

/// A browser hand-off: a session asking the person at a client to act in a
/// browser tab before it can carry on.
///
/// The parked shape crosses the socket as `BrowserHandOffPending` and the
/// dock draws it; `id` is what the answer comes back under, the same id the
/// registry is keyed by, and `tab` is the tab the person is asked to look at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandOff {
    pub id: uuid::Uuid,
    /// What the session needs done, in its own words.
    pub reason: String,
    /// The tab the person should see, from the asking session's own tabs.
    pub tab: u16,
}

/// The modifier keys a person held for one input event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputModifiers {
    pub alt: bool,
    pub control: bool,
    pub meta: bool,
    pub shift: bool,
}

/// One pointer gesture's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    Move,
    Press,
    Release,
}

/// One input event a controlling viewer sends, in CSS pixels for the page's
/// viewport.
///
/// **The wire carries the person's gesture, not the browser's protocol.** The
/// server is what turns a press into `Input.dispatchMouseEvent`; a client
/// speaks about a click, and the CDP method names stay on the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserInputEvent {
    Mouse {
        action: MouseAction,
        x: f64,
        y: f64,
        /// 0 left, 1 right, 2 middle.
        button: i32,
        clicks: i32,
        modifiers: InputModifiers,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        modifiers: InputModifiers,
    },
    /// A key with no text of its own: an arrow, Enter, Escape, a modifier.
    Key {
        down: bool,
        key: String,
        code: String,
        /// The Windows virtual key code a page's widgets read.
        windows_vk: i32,
        modifiers: InputModifiers,
    },
    /// The person's own text, inserted as text rather than replayed keycodes.
    Text {
        text: String,
    },
}

/// One encoded browser frame as the socket relays it.
///
/// **The bytes stay encoded end to end.** The host is the only encoder; the
/// server relays `encoded` untouched as one binary frame, and every client
/// decodes the same host frame the server read. `encoded` is the host's own
/// frame wire: the profile id, the tab, sequence, view size, format, key
/// flag, the page's own viewport and the WebP payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserFrame {
    /// The service's profile id: which surface the frame belongs to.
    pub profile: u16,
    /// The profile's tab this frame paints.
    pub tab: u16,
    /// Monotonic per tab; a newer frame supersedes an older one.
    pub seq: u32,
    pub encoded: Vec<u8>,
}

/// One tab as a client's browser strip draws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTabRow {
    /// The host's tab number, which frames and input already speak.
    pub number: u16,
    pub title: String,
    pub url: String,
    /// The session whose calls drive it.
    pub owner: crate::SessionSlot,
    pub state: BrowserTabState,
}

/// What a tab is doing, as the strip's mark says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTabState {
    /// A call is driving it now.
    InUse,
    Idle,
    /// A parked hand-off is waiting on the person in it.
    Held,
}

/// One profile's tabs and how the profile itself is doing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTabList {
    pub profile: String,
    /// The id the binary frames carry for the profile.
    pub id: u16,
    /// Whether its browser host is up.
    pub up: bool,
    /// Whether a driver is attached and running.
    pub connected: bool,
    /// When the idle clock closes it, as Unix epoch milliseconds.
    pub closing_at_ms: Option<u64>,
    pub rows: Vec<BrowserTabRow>,
}

/// One tab's viewer state, as every attached client sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserViewState {
    /// The controller's viewer id, when one holds the role on this tab.
    pub controller: Option<u64>,
    /// How many clients are attached to this tab.
    pub viewers: u32,
}

/// Where the page's history sits: what the takeover bar's two arrows may do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserNavState {
    pub can_back: bool,
    pub can_forward: bool,
}

/// A page's file chooser, as the client's picker reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserFileAsk {
    /// The dialog's id; the answer comes back under it.
    pub id: u64,
    /// The tab the dialog was raised on. **A viewer is asked only for its
    /// own tab**, and an answer from another tab is refused.
    pub tab: u16,
    /// Whether the page accepts more than one file.
    pub multiple: bool,
}

/// One file a person chose, on its way to the server's host as a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserUpload {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The most one file answer may carry across all its files, and the most
/// files it may name.
///
/// **The answer is base64 JSON in one socket message**, and the socket's
/// inbound cap is 16 MiB: 8 MiB of bytes encodes to about 10.7 MiB, so this
/// is a cap the connection really carries rather than a promise that tears
/// it. The client refuses past it before reading, and the server refuses
/// again before writing.
pub const MAX_UPLOAD_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_UPLOAD_FILES: usize = 16;

/// One finished download, on its way to the person's save door.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDownload {
    /// The tab the download was started from, so only its viewer is offered
    /// the save. **`None` is a profile-level notice**: the backlog dropped
    /// offers before a viewer read them, and the lost ones may belong to any
    /// tab of the profile.
    pub tab: Option<u16>,
    pub name: String,
    /// The file's bytes, empty when it could not be relayed.
    pub bytes: Vec<u8>,
    /// Why this download cannot be saved, when it cannot.
    pub error: Option<String>,
}

/// One packet of the page's audio, as the controlling client plays it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAudio {
    pub profile: u16,
    pub tab: u16,
    pub seq: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Interleaved 16-bit samples.
    pub samples: Vec<i16>,
}

/// Why a hand-off left the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandOffEnding {
    /// The person acted; the session carries on.
    Done,
    /// The person declined; the session knows what to do instead.
    NotNow,
    /// The person sent the session a message about the page and carried on:
    /// nothing is declined, and the session reads what they said.
    Feedback,
    /// The asking session went away before anyone answered.
    Abandoned,
}

/// Which door in a client produced a hand-off's answer.
///
/// **The ending says what it means; this says who said it.** A hand-off that
/// ends with nobody named is the defect two live rounds were spent on
/// (2026-10-07, 2026-10-26): the answer has to carry the hand that sent it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandOffSource {
    /// The dock's Done or Not now, before the takeover opened.
    Dock,
    /// The dock's own Escape, which is its refusal.
    DockEscape,
    /// The takeover bar's Done or Not now.
    Takeover,
    /// The takeover bar's feedback line: the person's words go to the session
    /// as a prompt, and the ending says they spoke.
    Feedback,
    /// The asking session's waiter went away; the core's own ending.
    Abandoned,
    /// A client that did not say which door it used.
    #[default]
    Unstated,
}

/// One answer to a parked browser hand-off: what it means, which door sent it,
/// and the connection that carried it.
///
/// `client` is stamped by the server at dispatch - a client cannot know its own
/// connection number - so every ending names the hand that sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandOffAnswer {
    pub ending: HandOffEnding,
    #[serde(default)]
    pub source: HandOffSource,
    #[serde(default)]
    pub client: Option<u64>,
}

/// Every browser tool the spec table advertises, by name.
///
/// The table itself lives in `forge-workspace`'s `mcp::browser::specs`, which
/// this crate may not depend on: a unit test there asserts the two lists
/// agree, and the live check in `forge-server`'s `surface_parity_live` test
/// asserts every name is answered by the pinned driver or by a host arm.
pub const ADVERTISED_TOOLS: [&str; 31] = [
    "browser_close",
    "browser_resize",
    "browser_console_messages",
    "browser_handle_dialog",
    "browser_emulate_media",
    "browser_evaluate",
    "browser_file_upload",
    "browser_drop",
    "browser_find",
    "browser_fill_form",
    "browser_press_key",
    "browser_type",
    "browser_navigate",
    "browser_navigate_back",
    "browser_network_requests",
    "browser_network_request",
    "browser_run_code_unsafe",
    "browser_take_screenshot",
    "browser_snapshot",
    "browser_click",
    "browser_drag",
    "browser_hover",
    "browser_select_option",
    "browser_tabs",
    "browser_wait_for",
    "browser_click_and_capture",
    "browser_form_state",
    "browser_challenge_state",
    "browser_hand_off",
    "browser_record_start",
    "browser_record_stop",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionSlot;

    fn key_of_default() -> ProfileKey {
        ProfileKey::of("Busytools", "default").expect("a key")
    }

    #[test]
    fn a_profile_key_is_two_safe_segments() {
        let key = ProfileKey::of("Busytools", "work").expect("a valid key");
        assert_eq!(key.as_str(), "Busytools/work");
        assert_eq!(key.org(), "Busytools");
        assert_eq!(key.name(), "work");
        assert_eq!(ProfileKey::parse("Busytools/default").expect("a parsed key"), key_of_default());

        for (key, why) in [
            ("Busytools", "one segment is not a key"),
            ("/work", "an empty org is not a key"),
            ("Busytools/", "an empty name is not a key"),
            ("Busy/tools/work", "a third segment is not a key"),
            ("Busytools/..", "a dot-dot name is not a key"),
            ("Busytools/.", "a dot name is not a key"),
            ("Busytools/two words", "a space is outside the name charset"),
            ("Busytools/a/b", "a slash is outside the name charset"),
        ] {
            assert!(ProfileKey::parse(key).is_err(), "{why}: {key}");
        }
        assert!(
            ProfileKey::of("Busy/tools", "work").is_err(),
            "an org carrying a slash is refused"
        );
    }

    /// A hand-off crosses as its own words plus the tab the person should
    /// look at.
    #[test]
    fn a_hand_off_names_the_tab_the_person_should_see() {
        let handoff =
            HandOff { id: uuid::Uuid::from_u128(7), reason: "CAPTCHA".to_owned(), tab: 3 };
        let encoded = serde_json::to_value(&handoff).expect("a hand-off encodes");
        assert_eq!(
            encoded,
            serde_json::json!({ "id": "00000000-0000-0000-0000-000000000007", "reason": "CAPTCHA", "tab": 3 }),
        );
        assert_eq!(serde_json::from_value::<HandOff>(encoded).expect("decodes"), handoff);
    }

    /// A tab row crosses with the session that drives it and what it is
    /// doing, so a strip can mark a held tab without a second read.
    #[test]
    fn a_tab_row_crosses_with_its_owner_and_state() {
        let row = BrowserTabRow {
            number: 2,
            title: "portal".to_owned(),
            url: "https://x/".to_owned(),
            owner: SessionSlot::worker("O", "p", "w"),
            state: BrowserTabState::Held,
        };
        let encoded = serde_json::to_value(&row).expect("a row encodes");
        assert_eq!(encoded["number"], 2);
        assert_eq!(encoded["state"], "held");
        assert_eq!(serde_json::from_value::<BrowserTabRow>(encoded).expect("decodes"), row);
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

    /// An answer crosses as the ending, the door that sent it and the
    /// connection that carried it, so a reader of the wire can name the hand
    /// rather than only the outcome.
    #[test]
    fn an_answer_crosses_with_its_source_and_client() {
        let answer = HandOffAnswer {
            ending: HandOffEnding::Feedback,
            source: HandOffSource::Feedback,
            client: Some(7),
        };
        let encoded = serde_json::to_value(answer).expect("an answer encodes");
        assert_eq!(
            encoded,
            serde_json::json!({
                "ending": { "type": "feedback" },
                "source": "feedback",
                "client": 7,
            }),
        );
        assert_eq!(serde_json::from_value::<HandOffAnswer>(encoded).expect("decodes"), answer);
        // A client cannot know its own connection number, so the field is
        // absent on the way in rather than a zero that would name a stranger -
        // and an answer that names no door still resolves the park, recorded
        // as unattributed rather than lost.
        assert_eq!(
            serde_json::from_value::<HandOffAnswer>(serde_json::json!({
                "ending": { "type": "done" },
            }))
            .expect("decodes without a source or a client"),
            HandOffAnswer {
                ending: HandOffEnding::Done,
                source: HandOffSource::Unstated,
                client: None,
            },
        );
    }

    /// **Every door crosses as the word the server writes, and the client's
    /// own tests assert the same four literals it writes.** The page builds
    /// them by hand: `dock` and `dock_escape` in `Dock.svelte`, `takeover`
    /// and `feedback` in `Composer.svelte`. The socket record walks keys and
    /// never values, so it cannot catch a rename on either side; this test
    /// and the client tests that click each door are what do, and a rename
    /// here without one there would leave the answer undecodable.
    #[test]
    fn every_client_door_crosses_as_the_word_the_client_writes() {
        for (source, word) in [
            (HandOffSource::Dock, "dock"),
            (HandOffSource::DockEscape, "dock_escape"),
            (HandOffSource::Takeover, "takeover"),
            (HandOffSource::Feedback, "feedback"),
        ] {
            assert_eq!(
                serde_json::to_value(source).expect("a source encodes"),
                serde_json::json!(word),
                "the client writes {word} for {source:?}",
            );
            assert_eq!(
                serde_json::from_value::<HandOffSource>(serde_json::json!(word))
                    .expect("the word decodes"),
                source,
                "{word} is the door it names",
            );
        }
    }

    /// The bytes are the part's, not its JSON form: a carrier that base64'd
    /// them into JSON would pay a third again for a screenshot.
    #[test]
    fn an_image_part_crosses_with_its_mime_and_not_its_bytes() {
        let part = BrowserPart::Image { mime_type: "image/png".to_owned(), bytes: vec![1, 2, 3] };
        let encoded = serde_json::to_value(&part).expect("a part encodes");
        assert_eq!(
            encoded,
            serde_json::json!({ "type": "image", "mime_type": "image/png" }),
            "the JSON form carries the tag and the mime, and never the bytes",
        );
        let BrowserPart::Image { mime_type, bytes } =
            serde_json::from_value(encoded).expect("a part decodes")
        else {
            panic!("an image part decodes into another part")
        };
        assert_eq!(mime_type, "image/png");
        assert!(bytes.is_empty(), "and a decoded JSON part carries no bytes");
    }

    /// **An input event crosses as the gesture, not as CDP.** A wire that
    /// named `Input.dispatchMouseEvent` would put the browser's protocol on
    /// every client; the server is what maps a gesture to it.
    #[test]
    fn an_input_event_crosses_as_the_gesture() {
        let click = BrowserInputEvent::Mouse {
            action: MouseAction::Press,
            x: 12.5,
            y: 30.0,
            button: 1,
            clicks: 1,
            modifiers: InputModifiers { meta: true, ..Default::default() },
        };
        let encoded = serde_json::to_value(&click).expect("an event encodes");
        assert_eq!(encoded["kind"], "mouse");
        assert_eq!(encoded["action"], "press");
        assert_eq!(encoded["modifiers"]["meta"], true);
        assert_eq!(
            serde_json::from_value::<BrowserInputEvent>(encoded).expect("an event decodes"),
            click,
        );
        let text = BrowserInputEvent::Text { text: "hi".to_owned() };
        assert_eq!(serde_json::to_value(&text).expect("text encodes")["kind"], "text");
        let key = BrowserInputEvent::Key {
            down: true,
            key: "Enter".to_owned(),
            code: "Enter".to_owned(),
            windows_vk: 13,
            modifiers: InputModifiers::default(),
        };
        assert_eq!(serde_json::to_value(&key).expect("a key encodes")["kind"], "key");
    }

    #[test]
    fn a_text_part_crosses_as_its_text() {
        let encoded = serde_json::to_value(BrowserPart::Text { text: "done".to_owned() })
            .expect("a part encodes");
        assert_eq!(encoded, serde_json::json!({ "type": "text", "text": "done" }));
    }
}
