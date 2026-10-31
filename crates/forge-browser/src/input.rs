//! Input as the page receives it: a controlling viewer's gesture, routed to
//! the part of the browser that can deliver it as a person's own.
//!
//! **A pointer gesture is the host's, a keystroke is the page's.** CEF
//! routes its own mouse calls into a windowless popup - a select's menu is a
//! separate view - and only `CefBrowserHost::SendMouse*Event` reaches that
//! routing; the same click sent to the page target over CDP dismisses the
//! menu instead of choosing from it. Keys and text stay on CDP, where a
//! page's trusted input is already right. The mapping is pure, so the shapes
//! are tested without a browser.

use forge_primitives::browser::{BrowserInputEvent, BrowserNavState, InputModifiers, MouseAction};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where the page's history sits, from a `Page.getNavigationHistory` reply.
pub fn history_state(reply: &Value) -> Option<BrowserNavState> {
    let (index, entries) = history_position(reply)?;
    Some(BrowserNavState { can_back: index > 0, can_forward: index + 1 < entries.len() })
}

/// The current index and the entries, when the reply holds both.
pub fn history_position(reply: &Value) -> Option<(usize, Vec<Value>)> {
    let index = usize::try_from(reply.get("currentIndex")?.as_i64()?).ok()?;
    let entries = reply.get("entries")?.as_array()?.clone();
    (index < entries.len()).then_some((index, entries))
}

/// Whether a page event asks the bar's history again. **Only the main
/// frame's navigation moves it**: a subframe's `frameNavigated` fires too,
/// and a read per ad iframe is nobody's time. A same-document entry moves no
/// frame and fires no load, so it names itself - and names the frame it
/// happened in, which is why `main` is checked for it too.
pub fn history_event(method: &str, event: &Value, main: Option<&str>) -> bool {
    match method {
        "Page.frameNavigated" => event.pointer("/params/frame/parentId").is_none(),
        "Page.loadEventFired" => true,
        // **An iframe's pushState is not the page's history.** The event
        // carries the frame it happened in; without this check a fragment
        // change inside an ad re-read the bar and overwrote the carry.
        "Page.navigatedWithinDocument" => event
            .pointer("/params/frameId")
            .and_then(Value::as_str)
            .is_some_and(|frame| Some(frame) == main),
        _ => false,
    }
}

/// The main frame's id, from a `Page.getFrameTree` reply.
pub fn main_frame_id(reply: &Value) -> Option<&str> {
    reply.pointer("/frameTree/frame/id").and_then(Value::as_str)
}

/// The main frame's id, from a parentless `Page.frameNavigated`.
pub fn navigated_frame_id(event: &Value) -> Option<&str> {
    if event.pointer("/params/frame/parentId").is_some() {
        return None;
    }
    event.pointer("/params/frame/id").and_then(Value::as_str)
}

/// The modifier mask CDP wants: Alt 1, Control 2, Meta 4, Shift 8.
pub fn cdp_modifiers(modifiers: InputModifiers) -> i32 {
    i32::from(modifiers.alt)
        | (i32::from(modifiers.control) << 1)
        | (i32::from(modifiers.meta) << 2)
        | (i32::from(modifiers.shift) << 3)
}

/// The modifier bits CEF's mouse events carry, from `cef_types.h`: shift 2,
/// control 4, alt 8, and Command 128 on macOS.
pub fn cef_modifiers(modifiers: InputModifiers) -> u32 {
    let mut bits = 0_u32;
    if modifiers.shift {
        bits |= 1 << 1;
    }
    if modifiers.control {
        bits |= 1 << 2;
    }
    if modifiers.alt {
        bits |= 1 << 3;
    }
    if modifiers.meta {
        bits |= 1 << 7;
    }
    bits
}

/// One pointer gesture as the host delivers it, in whole page pixels.
///
/// **CEF's mouse calls take view coordinates as integers**, so the fraction
/// is rounded here rather than in the host, and the line is one shape the
/// service and the host are both tested against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostInput {
    Mouse {
        /// The tab whose page takes the gesture.
        tab: u16,
        action: MouseAction,
        x: i32,
        y: i32,
        /// 0 left, 1 right, 2 middle - AppKit's own numbering.
        button: i32,
        clicks: i32,
        modifiers: InputModifiers,
    },
    Wheel {
        /// The tab whose page takes the gesture.
        tab: u16,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: InputModifiers,
    },
    /// Open a new tab at `url`, or blank. **A trusted server command**: unlike
    /// a page's popup it accepts any address the server names. `request` is
    /// echoed on the tab's birth so a caller can tell its tab from a popup
    /// that was queued beside it.
    TabNew {
        url: Option<String>,
        #[serde(default)]
        request: Option<u64>,
    },
    /// Close one tab.
    TabClose { tab: u16 },
    /// Lay one tab out apart from the profile's request: a take records at
    /// its own size without resizing every other tab.
    TabResize { tab: u16, width: u32, height: u32, scale_milli: u32 },
    /// The files a person chose for a page's file dialog: server-local paths
    /// already written for CEF, or none when the dialog was dismissed.
    File { id: u64, paths: Vec<String> },
    /// Answer a page's JavaScript dialog: `accept` is the button, and `prompt`
    /// the text a prompt asks for. **The host owns the CEF callback**, so an
    /// answer can only arrive this way (#2732).
    Dialog { tab: u16, accept: bool, prompt: Option<String> },
    /// Begin a recording of `tab`'s viewport into `path`, a server-local file
    /// the service has already laid the page out for.
    RecordStart { tab: u16, path: String },
    /// Finish the running recording; the host answers with its outcome.
    RecordStop,
}

/// What carries one input event.
#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    /// The host process, as one line on its own input pipe.
    Host(String),
    /// The page target over DevTools.
    Page(&'static str, Value),
}

/// The route one event takes, into the tab whose page the viewer is on.
///
/// **A pointer gesture belongs to the host, a keystroke to the page.** CEF
/// routes its own mouse calls into a windowless popup - a select's menu is
/// its own view - and a click sent to the page target instead reaches only
/// the page's widget, so the menu treats it as an outside click and closes.
pub fn route(event: &BrowserInputEvent, tab: u16) -> Route {
    match event {
        BrowserInputEvent::Mouse { action, x, y, button, clicks, modifiers } => {
            Route::Host(host_line(&HostInput::Mouse {
                tab,
                action: *action,
                x: rounded(*x),
                y: rounded(*y),
                button: *button,
                clicks: *clicks,
                modifiers: *modifiers,
            }))
        }
        BrowserInputEvent::Wheel { x, y, delta_x, delta_y, modifiers } => {
            // **The wire carries the page target's wheel sign; CEF's mouse
            // call takes the native one.** The client negates AppKit's
            // scrolling delta for CDP (where positive scrolls up), and CEF
            // forwards its delta unnegated, so it is turned back here or
            // every wheel - and every finger scroll on the phone - runs the
            // wrong way.
            Route::Host(host_line(&HostInput::Wheel {
                tab,
                x: rounded(*x),
                y: rounded(*y),
                delta_x: rounded(*delta_x).saturating_neg(),
                delta_y: rounded(*delta_y).saturating_neg(),
                modifiers: *modifiers,
            }))
        }
        BrowserInputEvent::Key { down, key, code, windows_vk, modifiers } => {
            let mut params = json!({
                "type": if *down { "keyDown" } else { "keyUp" },
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": windows_vk,
                "modifiers": cdp_modifiers(*modifiers),
            });
            // **Cmd+A/X/C/Z are editing commands, not printable keys.** A
            // keyDown without `commands` moves no selection and touches no
            // clipboard; Playwright sends them the same way.
            if *down && modifiers.meta {
                let command = match code.as_str() {
                    "KeyA" => Some("selectAll"),
                    "KeyC" => Some("copy"),
                    "KeyX" => Some("cut"),
                    "KeyZ" if modifiers.shift => Some("redo"),
                    "KeyZ" => Some("undo"),
                    _ => None,
                };
                if let Some(command) = command {
                    params["commands"] = json!([command]);
                }
            }
            Route::Page("Input.dispatchKeyEvent", params)
        }
        BrowserInputEvent::Text { text } => {
            Route::Page("Input.insertText", json!({ "text": text }))
        }
    }
}

/// One input as the line the host reads.
pub fn host_line(input: &HostInput) -> String {
    serde_json::to_string(input).unwrap_or_default()
}

/// The input one host line names, or `None` for a line it cannot read.
pub fn parse_host_line(line: &str) -> Option<HostInput> {
    serde_json::from_str(line).ok()
}

/// A page coordinate rounded to the whole pixel CEF's calls take.
fn rounded(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let value = value.round();
    if value <= f64::from(i32::MIN) {
        return i32::MIN;
    }
    if value >= f64::from(i32::MAX) {
        return i32::MAX;
    }
    #[expect(clippy::cast_possible_truncation, reason = "rounded and checked in range")]
    let at = value as i32;
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A click is the host's, never the page's.** The host's own mouse call
    /// is the only one CEF routes into a windowless popup; a click sent to
    /// the page target closes a select's menu instead of choosing from it.
    /// The line carries AppKit's button, the click count, the modifiers and
    /// whole pixels.
    #[test]
    fn a_click_goes_to_the_host_with_its_button_and_modifiers() {
        let route = route(
            &BrowserInputEvent::Mouse {
                action: MouseAction::Press,
                x: 12.5,
                y: 30.4,
                button: 1,
                clicks: 2,
                modifiers: InputModifiers { meta: true, shift: true, ..Default::default() },
            },
            1,
        );
        let Route::Host(line) = route else { panic!("a click is the host's: {route:?}") };
        let HostInput::Mouse { tab, action, x, y, button, clicks, modifiers } =
            parse_host_line(&line).expect("the host reads its own line")
        else {
            panic!("the line names a mouse event: {line}");
        };
        assert_eq!(tab, 1, "the click names the viewer's tab");
        assert_eq!((x, y), (13, 30), "the host is given whole pixels");
        assert_eq!((action, button, clicks), (MouseAction::Press, 1, 2));
        assert_eq!(cef_modifiers(modifiers), 130, "Command 128 + shift 2");
    }

    /// **A wheel is the host's too, with its sign turned back.** The wire
    /// carries the page target's convention (positive scrolls up) and CEF's
    /// mouse call takes the native one, so a wheel that skipped the flip
    /// scrolled the wrong way on every platform.
    #[test]
    fn a_wheel_carries_both_deltas_to_the_host_with_cefs_sign() {
        let route = route(
            &BrowserInputEvent::Wheel {
                x: 1.2,
                y: 2.0,
                delta_x: -3.0,
                delta_y: 40.0,
                modifiers: InputModifiers::default(),
            },
            1,
        );
        let Route::Host(line) = route else { panic!("a wheel is the host's: {route:?}") };
        let HostInput::Wheel { x, y, delta_x, delta_y, .. } =
            parse_host_line(&line).expect("the host reads its own line")
        else {
            panic!("the line names a wheel: {line}");
        };
        assert_eq!((x, y), (1, 2));
        assert_eq!(
            (delta_x, delta_y),
            (3, -40),
            "the wire's positive-up becomes CEF's positive-down"
        );
    }

    /// **A recording's two cues are the host's, and they round-trip.** The
    /// start carries the file the service prepared; a stop carries nothing,
    /// because the recording it names is the one running.
    #[test]
    fn a_recording_cue_round_trips_its_line() {
        let start = HostInput::RecordStart { tab: 1, path: "/tmp/take.mp4".to_owned() };
        assert_eq!(parse_host_line(&host_line(&start)), Some(start));
        assert_eq!(
            parse_host_line(&host_line(&HostInput::RecordStop)),
            Some(HostInput::RecordStop)
        );
    }

    /// **A gesture names the tab it belongs to, and the tab commands
    /// round-trip their lines.** One host serves several pages, so a click
    /// with no tab is a click into whichever page was created first.
    #[test]
    fn a_gesture_names_the_tab_it_belongs_to() {
        let line = host_line(&HostInput::Mouse {
            tab: 2,
            action: MouseAction::Press,
            x: 10,
            y: 20,
            button: 0,
            clicks: 1,
            modifiers: InputModifiers::default(),
        });
        let HostInput::Mouse { tab, .. } = parse_host_line(&line).expect("a gesture reads") else {
            panic!("a mouse line reads as a mouse");
        };
        assert_eq!(tab, 2);
        assert_eq!(
            parse_host_line(r#"{"kind":"tab_new","url":null}"#),
            Some(HostInput::TabNew { url: None, request: None })
        );
        assert_eq!(
            parse_host_line(r#"{"kind":"tab_close","tab":4}"#),
            Some(HostInput::TabClose { tab: 4 })
        );
    }

    /// **A line the host cannot read is refused, not guessed at.**
    #[test]
    fn an_unreadable_host_line_is_none() {
        assert!(parse_host_line("").is_none());
        assert!(parse_host_line("{\"kind\":\"mouse\"}").is_none());
        assert!(parse_host_line("{\"kind\":\"tap\"}").is_none());
    }

    /// **CEF's mask is not CDP's.** Command is bit 7 here where CDP has Meta
    /// at 4; a mask crossed over turns Shift+A into a selection shortcut.
    #[test]
    fn the_modifier_mask_is_cefs_own_bit_order() {
        assert_eq!(cef_modifiers(InputModifiers::default()), 0);
        assert_eq!(cef_modifiers(InputModifiers { shift: true, ..Default::default() }), 2);
        assert_eq!(cef_modifiers(InputModifiers { control: true, ..Default::default() }), 4);
        assert_eq!(cef_modifiers(InputModifiers { alt: true, ..Default::default() }), 8);
        assert_eq!(cef_modifiers(InputModifiers { meta: true, ..Default::default() }), 128);
        assert_eq!(
            cef_modifiers(InputModifiers { alt: true, control: true, meta: true, shift: true }),
            142,
        );
    }

    /// **Only the main frame's navigation asks the arrows again.** A link
    /// click commits the main frame; a subframe's navigation is the same
    /// event and must not pay a history read.
    #[test]
    fn a_link_navigation_asks_for_the_history_and_a_subframes_does_not() {
        let main = serde_json::json!({ "params": { "frame": { "id": "A" } } });
        let sub = serde_json::json!({ "params": { "frame": { "id": "B", "parentId": "A" } } });
        assert!(
            history_event("Page.frameNavigated", &main, Some("A")),
            "a link's navigation moves the bar"
        );
        assert!(!history_event("Page.frameNavigated", &sub, Some("A")), "a subframe's does not");
        assert!(history_event("Page.loadEventFired", &serde_json::json!({}), Some("A")));
        assert!(
            !history_event("Page.fileChooserOpened", &serde_json::json!({}), Some("A")),
            "a chooser is not a move"
        );
    }

    /// **A same-document move is news even though no frame navigates.** A
    /// fragment change or a pushState adds a history entry and fires only
    /// this event, so without it the Back arrow stays grey through it.
    #[test]
    fn a_same_document_move_asks_for_the_history() {
        let within =
            serde_json::json!({ "params": { "frameId": "A", "url": "https://example.com/" } });
        assert!(history_event("Page.navigatedWithinDocument", &within, Some("A")));
        assert!(
            !history_event("Page.navigatedWithinDocument", &within, Some("B")),
            "an iframe's move is not the page's",
        );
        assert!(
            !history_event("Page.navigatedWithinDocument", &within, None),
            "with no main frame known, no move is the page's yet",
        );
    }

    /// **The main frame's id is read from the tree and from its own
    /// navigation**, so a same-document event that names another frame can
    /// be told apart from the page's own move.
    #[test]
    fn the_main_frame_is_named_by_the_tree_and_its_own_navigation() {
        let tree = serde_json::json!({ "frameTree": { "frame": {
            "id": "MAIN",
            "children": [{ "frame": { "id": "AD", "parentId": "MAIN" } }],
        } } });
        assert_eq!(main_frame_id(&tree), Some("MAIN"));
        assert_eq!(
            navigated_frame_id(&serde_json::json!({ "params": { "frame": { "id": "MAIN" } } })),
            Some("MAIN"),
        );
        assert_eq!(
            navigated_frame_id(
                &serde_json::json!({ "params": { "frame": { "id": "AD", "parentId": "MAIN" } } })
            ),
            None,
            "a subframe's navigation does not name the main frame",
        );
    }

    /// **An editing chord is a command, not a printable key.** Without the
    /// command a keyDown moves no selection and touches no clipboard; the
    /// keyUp carries none. Keys stay on the page target.
    #[test]
    fn an_editing_chord_carries_its_command() {
        let key = |down, code: &str, shift| BrowserInputEvent::Key {
            down,
            key: code.to_owned(),
            code: code.to_owned(),
            windows_vk: 0,
            modifiers: InputModifiers { meta: true, shift, ..Default::default() },
        };
        let page = |event: &BrowserInputEvent| match route(event, 1) {
            Route::Page(_, params) => params,
            Route::Host(line) => panic!("a key is the page's, not the host's: {line}"),
        };
        assert_eq!(page(&key(true, "KeyA", false))["commands"], json!(["selectAll"]));
        assert_eq!(page(&key(true, "KeyZ", true))["commands"], json!(["redo"]));
        assert_eq!(page(&key(true, "KeyZ", false))["commands"], json!(["undo"]));
        assert!(
            page(&key(true, "KeyB", false)).get("commands").is_none(),
            "a printable key is not a command"
        );
        assert!(page(&key(false, "KeyA", false)).get("commands").is_none(), "an up has no command");
        let arrow = page(&key(true, "ArrowLeft", false));
        assert_eq!(arrow["type"], "keyDown");
        assert!(arrow.get("commands").is_none(), "a special key is not a chord");
    }

    /// **Text is inserted as text**, not replayed as keycodes: a layout the
    /// server does not know still types what the person typed.
    #[test]
    fn text_is_inserted_rather_than_replayed() {
        let route = route(&BrowserInputEvent::Text { text: "hello".to_owned() }, 1);
        let Route::Page(method, params) = route else {
            panic!("text is the page's: {route:?}");
        };
        assert_eq!(method, "Input.insertText");
        assert_eq!(params["text"], "hello");
    }

    #[test]
    fn the_modifier_mask_is_cdps_own_bit_order() {
        assert_eq!(cdp_modifiers(InputModifiers::default()), 0);
        assert_eq!(cdp_modifiers(InputModifiers { alt: true, ..Default::default() }), 1);
        assert_eq!(cdp_modifiers(InputModifiers { control: true, ..Default::default() }), 2);
        assert_eq!(cdp_modifiers(InputModifiers { meta: true, ..Default::default() }), 4);
        assert_eq!(cdp_modifiers(InputModifiers { shift: true, ..Default::default() }), 8);
        assert_eq!(
            cdp_modifiers(InputModifiers { alt: true, control: true, meta: true, shift: true }),
            15,
        );
    }
}
