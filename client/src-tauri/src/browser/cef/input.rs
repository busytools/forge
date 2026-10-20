//! Input as CDP events: the overlay view's events, mapped to what a page
//! treats as a person's own, and the page's own history, read for the
//! takeover's back and forward arrows and moved when either is pressed.
//!
//! **A page commits on trusted input, so the events go through CDP.** The
//! overlay's native events are the person's; the browser host has no window
//! to receive them, so they are replayed with `Input.dispatchMouseEvent`,
//! `Input.dispatchKeyEvent` and `Input.insertText` on the page target. The
//! mapping is pure where it can be, so the argv shapes are tested without a
//! browser.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tauri::Emitter as _;
use tokio::sync::mpsc;

use super::spawn;
use crate::browser::NavDirection;
use crate::browser::window::Cdp;

/// Where the page's history sits: what the two arrows may do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct History {
    pub can_back: bool,
    pub can_forward: bool,
}

/// The CDP `Page.getNavigationHistory` answer as the bar's two arrows.
pub fn history_state(reply: &Value) -> Option<History> {
    let (index, count) = history_position(reply)?;
    Some(History { can_back: index > 0, can_forward: index + 1 < count })
}

/// The current index and the number of entries, when the answer holds both.
fn history_position(reply: &Value) -> Option<(usize, usize)> {
    let index = usize::try_from(reply.get("currentIndex")?.as_i64()?).ok()?;
    let count = reply.get("entries")?.as_array()?.len();
    (index < count).then_some((index, count))
}

/// One event the overlay produced, in CSS pixels for the page's viewport.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    Mouse {
        kind: MouseKind,
        x: f64,
        y: f64,
        button: i32,
        clicks: i32,
        modifiers: i32,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        modifiers: i32,
    },
    /// A key with no text of its own: an arrow, Enter, Escape, a modifier.
    Key {
        down: bool,
        key: &'static str,
        code: &'static str,
        windows_vk: i32,
        modifiers: i32,
    },
    /// The person's own text, inserted as text rather than replayed keycodes.
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Move,
    Press,
    Release,
}

/// The CDP `Input.*` call one event is.
pub fn method_and_params(event: &InputEvent) -> (&'static str, Value) {
    match event {
        InputEvent::Mouse { kind, x, y, button, clicks, modifiers } => {
            let (method, type_) = match kind {
                MouseKind::Move => ("Input.dispatchMouseEvent", "mouseMoved"),
                MouseKind::Press => ("Input.dispatchMouseEvent", "mousePressed"),
                MouseKind::Release => ("Input.dispatchMouseEvent", "mouseReleased"),
            };
            // AppKit's own numbering: 0 left, 1 right, 2 middle.
            let button = match button {
                1 => "right",
                2 => "middle",
                _ => "left",
            };
            (
                method,
                json!({
                    "type": type_,
                    "x": x,
                    "y": y,
                    "button": button,
                    "clickCount": clicks,
                    "modifiers": modifiers,
                }),
            )
        }
        InputEvent::Wheel { x, y, delta_x, delta_y, modifiers } => (
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": x,
                "y": y,
                "deltaX": delta_x,
                "deltaY": delta_y,
                "modifiers": modifiers,
            }),
        ),
        InputEvent::Key { down, key, code, windows_vk, modifiers } => {
            let mut params = json!({
                "type": if *down { "keyDown" } else { "keyUp" },
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": windows_vk,
                "modifiers": modifiers,
            });
            // **Cmd+A/X/C/Z are editing commands, not printable keys.** A
            // keyDown without `commands` moves no selection and touches no
            // clipboard; Playwright sends them the same way.
            if *down && modifiers & 4 != 0 {
                let shift = modifiers & 8 != 0;
                let command = match *code {
                    "KeyA" => Some("selectAll"),
                    "KeyC" => Some("copy"),
                    "KeyX" => Some("cut"),
                    "KeyZ" if shift => Some("redo"),
                    "KeyZ" => Some("undo"),
                    _ => None,
                };
                if let Some(command) = command {
                    params["commands"] = json!([command]);
                }
            }
            ("Input.dispatchKeyEvent", params)
        }
        InputEvent::Text(text) => ("Input.insertText", json!({ "text": text })),
    }
}

/// The modifier mask CDP wants: Alt 1, Control 2, Meta 4, Shift 8.
pub fn cdp_modifiers(alt: bool, control: bool, meta: bool, shift: bool) -> i32 {
    i32::from(alt) | (i32::from(control) << 1) | (i32::from(meta) << 2) | (i32::from(shift) << 3)
}

/// The page's specials, by macOS virtual keycode: what a text insert cannot
/// express and a widget reads as a key.
pub fn special_key(keycode: u16) -> Option<(&'static str, &'static str, i32)> {
    // (DOM key, DOM code, Windows virtual key)
    Some(match keycode {
        36 => ("Enter", "Enter", 13),
        48 => ("Tab", "Tab", 9),
        51 => ("Backspace", "Backspace", 8),
        53 => ("Escape", "Escape", 27),
        115 => ("Home", "Home", 36),
        116 => ("PageUp", "PageUp", 33),
        117 => ("Delete", "Delete", 46),
        119 => ("End", "End", 35),
        121 => ("PageDown", "PageDown", 34),
        123 => ("ArrowLeft", "ArrowLeft", 37),
        124 => ("ArrowRight", "ArrowRight", 39),
        125 => ("ArrowDown", "ArrowDown", 40),
        126 => ("ArrowUp", "ArrowUp", 38),
        _ => return None,
    })
}

/// The longest a history read OR MOVE is given. Both ride the input task, so a
/// renderer that stops answering must not hold a keystroke behind either.
const PROBE_BOUND: Duration = Duration::from_secs(2);

/// What the surface asks its page connection to do.
pub(crate) enum Request {
    /// The person's own input, replayed as a trusted event.
    Input(InputEvent),
    /// The bar's arrows and their chords: move through the page's history.
    Navigate(NavDirection),
    /// A surface opened on a host that was already attached: publish the
    /// page's history again, past the change-only dedupe, because the bar
    /// resets its arrows on every open.
    RefreshHistory,
}

/// One CDP connection to the page, fed by the overlay's events.
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::UnboundedSender<Request>,
}

impl Sender {
    pub(crate) fn new(tx: mpsc::UnboundedSender<Request>) -> Self {
        Self { tx }
    }

    pub fn send(&self, event: InputEvent) {
        // A closed channel is a surface closing: dropping the event is right.
        let _ = self.tx.send(Request::Input(event));
    }

    pub fn navigate(&self, direction: NavDirection) {
        let _ = self.tx.send(Request::Navigate(direction));
    }

    pub fn refresh_history(&self) {
        let _ = self.tx.send(Request::RefreshHistory);
    }
}

/// One request for the person's file picker, answered with the chosen paths.
///
/// **The picker is the CLIENT's.** A page's file input clicked in the surface
/// is answered here rather than by CEF's own dialog: the client is the app
/// the person is looking at (its panel takes focus), and the chosen paths
/// are set on the input over CDP. CEF's windowless host has no window to
/// hang a panel on.
pub struct PanelRequest {
    pub reply: tokio::sync::oneshot::Sender<Option<Vec<String>>>,
}

/// Start the sender: resolve the page target, connect, and replay events
/// until the surface closes. **The history rides this connection too** - the
/// bar's state is pushed to the page as `browser_nav_state`, and a navigate
/// request moves the page with the same CDP connection the input uses.
pub async fn start(
    port: u16,
    cancel: Arc<AtomicBool>,
    panels: mpsc::UnboundedSender<PanelRequest>,
    app: tauri::AppHandle,
) -> Result<Sender, String> {
    let ws_url = spawn::page_websocket(port)
        .await
        .ok_or_else(|| "the browser host holds no page to send input to".to_owned())?;
    let prefix = spawn::ws_prefix(port);
    let path = ws_url
        .strip_prefix(&prefix)
        .ok_or_else(|| format!("the page's devtools endpoint is not on the host's port: {ws_url}"))?
        .to_owned();
    let (mut cdp, mut events) = Cdp::connect_watch(port, &path).await?;
    // **The page's own chooser, not a click probe.** CEF reports every file
    // chooser the page opens - a direct click on the input, a click on its
    // label, a scripted `input.click()` - so the client's panel answers all
    // of them. The probe saw only a pointer landing on the input itself and
    // missed the label and scripted forms a job site uses.
    if let Err(why) = cdp.call("Page.enable", json!({}), None).await {
        tauri_plugin_log::log::info!("the page's file choosers cannot be watched: {why}");
    } else if let Err(why) =
        cdp.call("Page.setInterceptFileChooserDialog", json!({ "enabled": true }), None).await
    {
        tauri_plugin_log::log::info!("the page's file-chooser interception did not enable: {why}");
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut last_history = None;
    push_history(&mut cdp, &app, &mut last_history).await;
    tauri::async_runtime::spawn(async move {
        loop {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            tokio::select! {
                request = rx.recv() => {
                    let Some(request) = request else { break };
                    match request {
                        Request::Input(event) => {
                            let (method, params) = method_and_params(&event);
                            // **Sent, not awaited**: a round trip per
                            // keystroke is lag, and a drag's moves queue
                            // behind one another if each waits its reply.
                            if let Err(why) = cdp.send(method, params).await {
                                tauri_plugin_log::log::info!(
                                    "an input event did not land: {why}"
                                );
                            }
                        }
                        Request::Navigate(direction) => {
                            if let Err(why) = navigate_history(&mut cdp, direction).await {
                                tauri_plugin_log::log::info!(
                                    "the page did not move through its history (event_name \
                                     cef_history_navigate_failed): {why}"
                                );
                            }
                            push_history(&mut cdp, &app, &mut last_history).await;
                        }
                        Request::RefreshHistory => {
                            // The bar cleared its arrows on this open, so the
                            // pair is news even when the shell last sent it.
                            last_history = None;
                            push_history(&mut cdp, &app, &mut last_history).await;
                        }
                    }
                }
                event = events.recv() => {
                    let Some(event) = event else { break };
                    match event.get("method").and_then(Value::as_str) {
                        Some("Page.fileChooserOpened") => {
                            let Some(node) =
                                event.pointer("/params/backendNodeId").and_then(Value::as_i64)
                            else {
                                continue;
                            };
                            if let Some(paths) = pick_files(&panels).await {
                                set_files(&mut cdp, node, &paths).await;
                            }
                        }
                        Some(method) if history_event(method, &event) => {
                            push_history(&mut cdp, &app, &mut last_history).await;
                        }
                        _ => {}
                    }
                }
            }
            // **A close that landed while an event was in hand is still a
            // close.** The check above is the loop's guard; this one keeps a
            // surface that went away mid-event from processing one more.
            if cancel.load(Ordering::Acquire) {
                break;
            }
        }
    });
    Ok(Sender::new(tx))
}

/// Whether a page event asks the bar's history again. **Only the main
/// frame's navigation moves it**: a subframe's `frameNavigated` fires too,
/// and a read per ad iframe is the input task's time. A same-document entry
/// moves no frame and fires no load, so it names itself.
fn history_event(method: &str, event: &Value) -> bool {
    match method {
        "Page.frameNavigated" => event.pointer("/params/frame/parentId").is_none(),
        "Page.loadEventFired" | "Page.navigatedWithinDocument" => true,
        _ => false,
    }
}

/// What a history read should publish, or `None` when it is no news.
///
/// **A read that fails is no news.** The read right after a
/// `navigateToHistoryEntry` can land before the new frame is attached
/// (`-32000 "Not attached to an active page"`); publishing that as a
/// both-disabled pair would grey the arrows until the next frame, so a failed
/// or malformed answer leaves the last state standing. Pure, so that rule is
/// pinned without a devtools endpoint.
fn history_news(reply: Option<&Value>, last: Option<History>) -> Option<History> {
    let history = history_state(reply?)?;
    (last != Some(history)).then_some(history)
}

/// One bounded history read: `Some` only for an answer inside the bound.
/// **The bound rides the input task**, so a wedged renderer cannot hold a
/// keystroke behind it; a call that fails and one that never answers are both
/// no news.
async fn history_reply(
    read: impl std::future::Future<Output = Result<Value, String>>,
    bound: Duration,
) -> Option<Value> {
    match tokio::time::timeout(bound, read).await {
        Ok(Ok(reply)) => Some(reply),
        Ok(Err(why)) => {
            tauri_plugin_log::log::info!(
                "the page's history could not be read (event_name cef_nav_state_unread): {why}"
            );
            None
        }
        Err(_) => {
            tauri_plugin_log::log::info!(
                "the page's history did not answer within {bound:?} (event_name \
                 cef_nav_state_timeout)"
            );
            None
        }
    }
}

/// One bounded move: the same bound as the read, because the move rides the
/// same input task - a click the person already made must not sit behind a
/// renderer that stopped answering.
async fn history_move(
    move_it: impl std::future::Future<Output = Result<Value, String>>,
    bound: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(bound, move_it).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => Err(format!("the page did not accept the move within {bound:?}")),
    }
}

/// Read the page's history and tell the bar what it may do. **Emitted only
/// on a change**: a cross-document navigation fires both the frame and the
/// load event, and an unchanging pair is not news.
async fn push_history(cdp: &mut Cdp, app: &tauri::AppHandle, last: &mut Option<History>) {
    let reply =
        history_reply(cdp.call("Page.getNavigationHistory", json!({}), None), PROBE_BOUND).await;
    let Some(history) = history_news(reply.as_ref(), *last) else {
        return;
    };
    *last = Some(history);
    tauri_plugin_log::log::info!(
        "the page's history moved (event_name cef_nav_state, back {}, forward {})",
        history.can_back,
        history.can_forward,
    );
    let _ = app.emit("browser_nav_state", history);
}

/// The id of the entry one step `direction` from where the page sits, when
/// that step exists and names one. The position is the caller's parse, so a
/// move reads the answer once.
fn history_target(
    reply: &Value,
    index: usize,
    count: usize,
    direction: NavDirection,
) -> Option<i64> {
    let next = match direction {
        NavDirection::Back => index.checked_sub(1),
        NavDirection::Forward => (index + 1 < count).then_some(index + 1),
    };
    next.and_then(|next| reply.get("entries")?.get(next))
        .and_then(|entry| entry.get("id"))
        .and_then(Value::as_i64)
}

/// Move the page to the entry next to where it sits, when there is one.
///
/// **A click during a navigation is a no-op**: the history read lands before
/// the frame attaches and names no entry, and the next navigation re-enables
/// the arrows. A retry queue would carry state for a window that closes by
/// itself.
async fn navigate_history(cdp: &mut Cdp, direction: NavDirection) -> Result<(), String> {
    // **Both calls are bounded like the probe.** This runs on the input
    // task, and a renderer that stops answering must not hold a keystroke
    // behind a click the person already made.
    let Some(reply) =
        history_reply(cdp.call("Page.getNavigationHistory", json!({}), None), PROBE_BOUND).await
    else {
        return Ok(());
    };
    let Some((index, count)) = history_position(&reply) else {
        tauri_plugin_log::log::info!(
            "the page's history could not be read (event_name cef_history_navigate_unread)"
        );
        return Ok(());
    };
    let Some(entry_id) = history_target(&reply, index, count, direction) else {
        return Ok(());
    };
    history_move(
        cdp.call("Page.navigateToHistoryEntry", json!({ "entryId": entry_id }), None),
        PROBE_BOUND,
    )
    .await
}

/// Ask the client for the person's picker and answer the chosen paths.
async fn pick_files(panels: &mpsc::UnboundedSender<PanelRequest>) -> Option<Vec<String>> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    panels.send(PanelRequest { reply }).ok()?;
    match rx.await {
        Ok(Some(paths)) if !paths.is_empty() => Some(paths),
        _ => None,
    }
}

/// Set the picked files on the input the chooser opened, which fires the
/// page's own `input` and `change` events.
async fn set_files(cdp: &mut Cdp, backend_node_id: i64, paths: &[String]) {
    if let Err(why) = cdp
        .call(
            "DOM.setFileInputFiles",
            json!({ "files": paths, "backendNodeId": backend_node_id }),
            None,
        )
        .await
    {
        tauri_plugin_log::log::info!("the chosen files did not land: {why}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Cmd+A/X/C/Z carry the editing command, and only on the down.** A
    /// keyDown with no `commands` selects nothing and touches no clipboard;
    /// a keyUp carrying one would fire the command twice.
    #[test]
    fn an_editing_shortcut_carries_its_command_once() {
        let key = |down: bool, code: &'static str, modifiers: i32| {
            let (_, params) = method_and_params(&InputEvent::Key {
                down,
                key: "a",
                code,
                windows_vk: 65,
                modifiers,
            });
            params.get("commands").cloned()
        };
        assert_eq!(key(true, "KeyA", 4), Some(json!(["selectAll"])), "Cmd+A selects all");
        assert_eq!(key(true, "KeyC", 4), Some(json!(["copy"])), "Cmd+C copies");
        assert_eq!(key(true, "KeyX", 4), Some(json!(["cut"])), "Cmd+X cuts");
        assert_eq!(key(true, "KeyZ", 4), Some(json!(["undo"])), "Cmd+Z undoes");
        assert_eq!(key(true, "KeyZ", 4 | 8), Some(json!(["redo"])), "Shift+Cmd+Z redoes");
        assert_eq!(key(false, "KeyA", 4), None, "the up carries no command");
        assert_eq!(key(true, "KeyA", 0), None, "a plain A is text, not a command");
    }

    #[test]
    fn a_press_is_a_left_button_press_with_its_clicks() {
        let event = InputEvent::Mouse {
            kind: MouseKind::Press,
            x: 12.0,
            y: 34.0,
            button: 0,
            clicks: 2,
            modifiers: 0,
        };
        let (method, params) = method_and_params(&event);
        assert_eq!(method, "Input.dispatchMouseEvent");
        assert_eq!(params["type"], "mousePressed");
        assert_eq!(params["button"], "left");
        assert_eq!(params["clickCount"], 2);
    }

    #[test]
    fn the_modifier_mask_is_cdp_s_own() {
        assert_eq!(cdp_modifiers(false, false, false, false), 0);
        assert_eq!(cdp_modifiers(true, false, false, false), 1, "alt");
        assert_eq!(cdp_modifiers(false, true, false, false), 2, "control");
        assert_eq!(cdp_modifiers(false, false, true, false), 4, "meta");
        assert_eq!(cdp_modifiers(false, false, false, true), 8, "shift");
        assert_eq!(cdp_modifiers(true, true, true, true), 15, "all four");
    }

    #[test]
    fn a_special_key_carries_its_code_and_virtual_key() {
        let (key, code, vk) = special_key(36).expect("enter is a special");
        assert_eq!((key, code, vk), ("Enter", "Enter", 13));
        let (key, _, vk) = special_key(126).expect("up is a special");
        assert_eq!((key, vk), ("ArrowUp", 38));
        assert_eq!(special_key(0), None, "a text key is not a special");
    }

    #[test]
    fn text_is_inserted_not_replayed() {
        let (method, params) = method_and_params(&InputEvent::Text("hello".to_owned()));
        assert_eq!(method, "Input.insertText");
        assert_eq!(params["text"], "hello");
    }

    #[test]
    fn a_wheel_is_a_mouse_wheel_with_both_deltas() {
        let event =
            InputEvent::Wheel { x: 1.0, y: 2.0, delta_x: 3.0, delta_y: -40.0, modifiers: 0 };
        let (method, params) = method_and_params(&event);
        assert_eq!(method, "Input.dispatchMouseEvent");
        assert_eq!(params["type"], "mouseWheel");
        assert_eq!(params["deltaY"], -40.0);
    }

    /// **The arrows follow the page's own history, not a guess.** CDP
    /// answers the current index and the entries; back is available at any
    /// index but the first, forward at any but the last.
    #[test]
    fn the_arrows_follow_the_page_s_own_history() {
        let at = |index: i64, entries: usize| {
            let entries: Vec<Value> = (0..entries).map(|n| json!({ "id": n + 1 })).collect();
            history_state(&json!({ "currentIndex": index, "entries": entries }))
        };
        assert_eq!(
            at(0, 1),
            Some(History { can_back: false, can_forward: false }),
            "a fresh page goes nowhere"
        );
        assert_eq!(
            at(1, 3),
            Some(History { can_back: true, can_forward: true }),
            "an entry between two others goes both ways"
        );
        assert_eq!(
            at(2, 3),
            Some(History { can_back: true, can_forward: false }),
            "the last entry has only back"
        );
        assert_eq!(
            at(0, 3),
            Some(History { can_back: false, can_forward: true }),
            "after going back to the first entry, only forward"
        );
        assert_eq!(
            history_state(&json!({ "entries": [{ "id": 1 }] })),
            None,
            "an answer with no index is not a state"
        );
        assert_eq!(at(3, 3), None, "an index AT the count names no entry, so no state either");
        assert_eq!(
            history_state(&json!({ "currentIndex": 4, "entries": [] })),
            None,
            "an index past the entries is not a state"
        );
    }

    /// **The move names the entry one step away, and neither step when there
    /// is none.** Swapping Back and Forward must fail here, not only in the
    /// chord table.
    #[test]
    fn the_neighbour_is_the_entry_one_step_away() {
        let history = json!({
            "currentIndex": 1,
            "entries": [{ "id": 11 }, { "id": 22 }, { "id": 33 }],
        });
        // The position comes from the same parse the shell uses, so a change
        // there flows into this pin rather than around it.
        let (index, count) = history_position(&history).expect("the answer parses");
        assert_eq!(
            history_target(&history, index, count, NavDirection::Back),
            Some(11),
            "back is the entry before"
        );
        assert_eq!(
            history_target(&history, index, count, NavDirection::Forward),
            Some(33),
            "forward is the entry after"
        );

        let first = json!({ "currentIndex": 0, "entries": [{ "id": 11 }, { "id": 22 }] });
        let (index, count) = history_position(&first).expect("the answer parses");
        assert_eq!(
            history_target(&first, index, count, NavDirection::Back),
            None,
            "the first entry has no back"
        );
        assert_eq!(history_target(&first, index, count, NavDirection::Forward), Some(22));

        let last = json!({ "currentIndex": 1, "entries": [{ "id": 11 }, { "id": 22 }] });
        let (index, count) = history_position(&last).expect("the answer parses");
        assert_eq!(
            history_target(&last, index, count, NavDirection::Forward),
            None,
            "the last has no forward"
        );
        assert_eq!(history_target(&last, index, count, NavDirection::Back), Some(11));
    }

    /// **A read that is no news publishes nothing.** The failing read right
    /// after a move (`-32000 "Not attached to an active page"`) must leave the
    /// bar's pair standing: folding it back into a default pair is the
    /// regression this pins.
    #[test]
    fn a_read_that_is_no_news_publishes_nothing() {
        let fresh = History { can_back: false, can_forward: false };
        let moved = History { can_back: true, can_forward: false };
        assert_eq!(history_news(None, Some(moved)), None, "a failed read leaves the pair standing");
        let unreadable = json!({ "currentIndex": 4, "entries": [] });
        assert_eq!(history_news(Some(&unreadable), Some(moved)), None, "an unreadable answer too");
        let same = json!({ "currentIndex": 2, "entries": [{ "id": 1 }, { "id": 2 }, { "id": 3 }] });
        assert_eq!(
            history_news(Some(&same), Some(moved)),
            None,
            "the pair the bar already shows is not news"
        );
        assert_eq!(history_news(Some(&same), Some(fresh)), Some(moved), "a changed pair is news");
        assert_eq!(history_news(Some(&same), None), Some(moved), "the first read is news");
    }

    /// **Only the main frame's navigation asks for the history again.** A
    /// subframe's frameNavigated is the same event and must not pay a read.
    #[test]
    fn only_the_main_frames_navigation_asks_for_the_history() {
        let main = json!({ "params": { "frame": { "id": "A" } } });
        let sub = json!({ "params": { "frame": { "id": "B", "parentId": "A" } } });
        assert!(history_event("Page.frameNavigated", &main), "the main frame moves the history");
        assert!(!history_event("Page.frameNavigated", &sub), "a subframe does not");
        assert!(history_event("Page.loadEventFired", &json!({})));
        assert!(history_event("Page.navigatedWithinDocument", &json!({})));
        assert!(!history_event("Page.fileChooserOpened", &json!({})), "a chooser is not a move");
    }

    /// **A read that never answers is no news, and the bound is what says
    /// so.** This rides the input task, so a wedged renderer must not hold a
    /// keystroke behind the probe.
    #[tokio::test]
    async fn a_history_read_that_never_answers_is_no_news() {
        assert_eq!(PROBE_BOUND, Duration::from_secs(2), "the bound the input task rides is two s");
        let pending = std::future::pending::<Result<Value, String>>();
        let started = std::time::Instant::now();
        assert!(
            history_reply(pending, Duration::from_millis(10)).await.is_none(),
            "a read past its bound is no state"
        );
        assert!(started.elapsed() < Duration::from_secs(1), "the bound decided, not the future");
    }

    /// **A move that never answers is named, not waited on.** The move rides
    /// the input task like the read; a bare call here would hold a click the
    /// person already made.
    #[tokio::test]
    async fn a_history_move_that_never_answers_is_named() {
        let pending = std::future::pending::<Result<Value, String>>();
        let why = history_move(pending, Duration::from_millis(10))
            .await
            .expect_err("a move past its bound fails");
        assert!(why.contains("did not accept the move"), "{why}");
    }

    /// **An open asks the history again through the surface's request**, on
    /// the same channel input rides.
    #[test]
    fn an_open_asks_the_history_again() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sender = Sender::new(tx);
        sender.refresh_history();
        assert!(matches!(rx.try_recv(), Ok(Request::RefreshHistory)), "the replay request is sent");
        sender.send(InputEvent::Text("x".to_owned()));
        assert!(
            matches!(rx.try_recv(), Ok(Request::Input(_))),
            "input still rides the same channel"
        );
    }
}
