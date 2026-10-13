//! The window as the person sees it: where a launch put it, and the two moves
//! this host makes on it - out of sight, and forward.
//!
//! **The launch position is not one of the moves.** Measured 2026-10-12
//! (macOS, Brave 155): `--window-position=-9999,-9999` was clamped by the
//! window server to the top-left of the screen - bounds read back (0, 30) -
//! so a launch cannot be told where its window goes. The window appears, and
//! hiding it is this module's job, spoken over the browser's own devtools
//! endpoint: the port file names the endpoint, and no launch flag can do it.
//!
//! The moves are the two the host needs and no more: [`move_to`] is the hide
//! (a NORMAL window placed clear of every screen, its page staying awake),
//! and [`bring_forward`] is the raise - the window back to normal, placed,
//! and made the active one. [`minimize`] is kept as the counterfactual the
//! design was measured against: it hides perfectly and freezes the page, so
//! the driver's click dies behind it, and whoever is tempted to switch the
//! hide to it should read the live test named in its own doc first - do NOT
//! cut it as dead surface.

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

/// A window as the browser reports it: the browser's own window id and the
/// bounds it answers with.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub id: i64,
    /// The bounds as answered: `left`, `top`, `width`, `height`,
    /// `windowState` (`normal` / `minimized` / `maximized` / `fullscreen`).
    pub bounds: Value,
}

impl Window {
    /// One numeric bound, when the browser answered one.
    pub fn number(&self, name: &str) -> Option<i64> {
        self.bounds.get(name).and_then(Value::as_i64)
    }

    /// The window state the browser reports, when it reports one.
    pub fn state(&self) -> Option<&str> {
        self.bounds.get("windowState").and_then(Value::as_str)
    }
}

/// One connection to a browser's devtools endpoint, speaking just enough CDP
/// for the window moves: numbered calls, replies matched by id, the frames
/// between them skipped.
pub struct Cdp {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl Cdp {
    /// Connect to the endpoint a port file names.
    pub async fn connect(port: u16, path: &str) -> Result<Self, String> {
        let url = format!("ws://127.0.0.1:{port}{path}");
        let (stream, _) = tokio_tungstenite::connect_async(&url).await.map_err(|why| {
            format!("the browser's devtools endpoint at {url} did not accept: {why}")
        })?;
        Ok(Self { stream, next_id: 0 })
    }

    /// One call, answered: the reply's `result`, or the protocol error's own
    /// sentence. Frames that are not this call's reply are skipped - events
    /// and other replies do not concern a caller that asked one question.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_owned());
        }
        self.stream
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|why| format!("{method} could not be sent: {why}"))?;
        loop {
            let frame = self
                .stream
                .next()
                .await
                .ok_or_else(|| {
                    format!("{method}: the devtools endpoint closed before it answered")
                })?
                .map_err(|why| format!("{method}: the devtools endpoint failed: {why}"))?;
            let Message::Text(text) = frame else { continue };
            let Ok(reply) = serde_json::from_str::<Value>(&text) else { continue };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return match reply.get("error") {
                Some(error) => Err(format!(
                    "{method}: {}",
                    error.get("message").and_then(Value::as_str).unwrap_or("refused")
                )),
                None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
            };
        }
    }
}

/// The first page target's id, and a session attached to it, for the
/// `Page`-domain commands that only speak to a page.
pub struct Page {
    pub target: String,
    pub session: String,
}

impl Cdp {
    /// Attach to the first page the browser holds. A browser with no page -
    /// a launch that never opened one - is a refusal naming that, not a
    /// panic: there is no window to move without a page under it.
    ///
    /// **The first page is THE page, and that is an assumption worth
    /// naming.** Every profile forge drives holds one tab - the window is
    /// born on the page the last raise carried, and nothing here opens
    /// another - so the first target is the one the session drove and the
    /// raise brings the right window forward. A profile that ever grows a
    /// second tab (a session calling `browser_tabs` with a new tab, say)
    /// would need its tab chosen deliberately on the raise, which is filed
    /// as its own piece rather than grown into the raise here.
    pub async fn page(&mut self) -> Result<Page, String> {
        let targets = self.call("Target.getTargets", json!({}), None).await?;
        let target = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .and_then(|targets| {
                targets
                    .iter()
                    .find(|entry| entry.get("type").and_then(Value::as_str) == Some("page"))
            })
            .and_then(|entry| entry.get("targetId"))
            .and_then(Value::as_str)
            .ok_or("the browser holds no page to act on")?
            .to_owned();
        let attached = self
            .call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)
            .await?;
        let session = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("the browser refused the page session")?
            .to_owned();
        Ok(Page { target, session })
    }
}

/// The window a page lives in, as the browser reports it.
pub async fn window(page: &Page, cdp: &mut Cdp) -> Result<Window, String> {
    let answer =
        cdp.call("Browser.getWindowForTarget", json!({ "targetId": page.target }), None).await?;
    let id = answer
        .get("windowId")
        .and_then(Value::as_i64)
        .ok_or("the browser named no window for the page (a headless launch has none)")?;
    Ok(Window { id, bounds: answer.get("bounds").cloned().unwrap_or(Value::Null) })
}

/// Put the window out of sight by moving it: a NORMAL window placed clear of
/// every screen, so the page stays a visible page (frames and input live) and
/// only the person's eye is spared.
pub async fn move_to(page: &Page, cdp: &mut Cdp, left: i64, top: i64) -> Result<(), String> {
    let window = window(page, cdp).await?;
    cdp.call(
        "Browser.setWindowBounds",
        json!({ "windowId": window.id, "bounds": { "left": left, "top": top } }),
        None,
    )
    .await
    .map(|_| ())
}

/// Put the window out of sight by minimizing it.
///
/// **Not the hide, and kept on purpose.** Measured 2026-10-12: a minimized
/// window's page reads hidden and its animation frames stop, so the
/// vendored driver's click times out against it (five seconds, `force`
/// included) - and neither `--disable-backgrounding-occluded-windows` nor
/// `Page.setWebLifecycleState('active')` revives the frames. This is the
/// counterfactual the off-screen move was chosen over;
/// `a_minimized_window_can_still_be_driven` in the live suite pins it.
pub async fn minimize(page: &Page, cdp: &mut Cdp) -> Result<(), String> {
    let window = window(page, cdp).await?;
    cdp.call(
        "Browser.setWindowBounds",
        json!({ "windowId": window.id, "bounds": { "windowState": "minimized" } }),
        None,
    )
    .await
    .map(|_| ())
}

/// Bring the window forward: normal state, placed where the caller says, and
/// made the browser's active window.
pub async fn bring_forward(page: &Page, cdp: &mut Cdp, left: i64, top: i64) -> Result<(), String> {
    let window = window(page, cdp).await?;
    cdp.call(
        "Browser.setWindowBounds",
        json!({
            "windowId": window.id,
            "bounds": { "windowState": "normal", "left": left, "top": top },
        }),
        None,
    )
    .await?;
    cdp.call("Page.bringToFront", json!({}), Some(&page.session)).await.map(|_| ())
}
