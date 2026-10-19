//! Input as CDP events: the overlay view's events, mapped to what a page
//! treats as a person's own.
//!
//! **A page commits on trusted input, so the events go through CDP.** The
//! overlay's native events are the person's; the browser host has no window
//! to receive them, so they are replayed with `Input.dispatchMouseEvent`,
//! `Input.dispatchKeyEvent` and `Input.insertText` on the page target. The
//! mapping is pure where it can be, so the argv shapes are tested without a
//! browser.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::spawn;
use crate::browser::window::Cdp;

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

/// One CDP connection to the page, fed by the overlay's events.
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::UnboundedSender<InputEvent>,
}

impl Sender {
    pub fn send(&self, event: InputEvent) {
        // A closed channel is a surface closing: dropping the event is right.
        let _ = self.tx.send(event);
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

/// The marker the click probe puts on the input it found, so the set-files
/// call can address it without an id the page may not have.
const UPLOAD_MARK: &str = "data-forge-upload";

/// Start the sender: resolve the page target, connect, and replay events
/// until the surface closes.
pub async fn start(
    port: u16,
    cancel: Arc<AtomicBool>,
    panels: mpsc::UnboundedSender<PanelRequest>,
) -> Result<Sender, String> {
    let ws_url = spawn::page_websocket(port)
        .await
        .ok_or_else(|| "the browser host holds no page to send input to".to_owned())?;
    let prefix = spawn::ws_prefix(port);
    let path = ws_url
        .strip_prefix(&prefix)
        .ok_or_else(|| format!("the page's devtools endpoint is not on the host's port: {ws_url}"))?
        .to_owned();
    let mut cdp = Cdp::connect(port, &path).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    tauri::async_runtime::spawn(async move {
        while !cancel.load(Ordering::Acquire) {
            let Some(event) = rx.recv().await else { break };
            let (method, params) = method_and_params(&event);
            // **Sent, not awaited**: a round trip per keystroke is lag, and a
            // drag's moves queue behind one another if each waits its reply.
            if let Err(why) = cdp.send(method, params).await {
                tauri_plugin_log::log::info!("an input event did not land: {why}");
            }
            // **A click on a file input is the person asking for their
            // picker, and the click has already gone.** The probe is a round
            // trip, so it must not sit in front of every click; it runs after
            // the click landed, marks the input, and sets the chosen files on
            // it below.
            if let InputEvent::Mouse { kind: MouseKind::Press, x, y, .. } = event
                && let Some(paths) = pick_files(&mut cdp, &panels, x, y).await
            {
                set_files(&mut cdp, &paths).await;
            }
        }
    });
    Ok(Sender { tx })
}

/// Whether the click is on a file input; when it is, asks the client for the
/// person's picker and answers the chosen paths.
async fn pick_files(
    cdp: &mut Cdp,
    panels: &mpsc::UnboundedSender<PanelRequest>,
    x: f64,
    y: f64,
) -> Option<Vec<String>> {
    let probe = format!(
        "(() => {{ const el = document.elementFromPoint({x}, {y}); \
         if (!el || el.tagName !== 'INPUT' || el.type !== 'file') return false; \
         el.setAttribute('{UPLOAD_MARK}', '1'); return true; }})()"
    );
    let answer = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": probe, "returnByValue": true, "userGesture": true }),
            None,
        )
        .await
        .ok()?;
    if answer.pointer("/result/value").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let (reply, rx) = tokio::sync::oneshot::channel();
    panels.send(PanelRequest { reply }).ok()?;
    match rx.await {
        Ok(Some(paths)) if !paths.is_empty() => Some(paths),
        _ => None,
    }
}

/// Set the picked files on the marked input, which fires its own `input` and
/// `change` events; the mark is removed either way.
async fn set_files(cdp: &mut Cdp, paths: &[String]) {
    let document = cdp.call("DOM.getDocument", json!({}), None).await;
    let root = document.ok().and_then(|doc| doc.pointer("/root/nodeId").and_then(Value::as_i64));
    if let Some(root) = root {
        let found = cdp
            .call(
                "DOM.querySelector",
                json!({ "nodeId": root, "selector": format!("input[{UPLOAD_MARK}]") }),
                None,
            )
            .await
            .ok()
            .and_then(|answer| answer.get("nodeId").and_then(Value::as_i64));
        if let Some(node) = found
            && let Err(why) = cdp
                .call("DOM.setFileInputFiles", json!({ "files": paths, "nodeId": node }), None)
                .await
        {
            tauri_plugin_log::log::info!("the chosen files did not land: {why}");
        }
    }
    let _ = cdp
        .call(
            "Runtime.evaluate",
            json!({
                "expression": format!(
                    "document.querySelector('input[{UPLOAD_MARK}]')?.removeAttribute('{UPLOAD_MARK}')"
                ),
            }),
            None,
        )
        .await;
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
}
