//! The host opener bridge's shared state and origin rules.
//!
//! **A cross-site `window.open` is not a CEF popup.** The host creates a
//! browser of its own at the requested URL, and the page's `window.open`
//! answers a proxy the host relays through: `postMessage` in both directions,
//! `closed`, `close`, navigation, and `window.opener` on the popup's side.
//! This module is the state that pairs the two tabs and the origin checks the
//! relay obeys. It holds no CEF types, so the rules are unit-tested without a
//! host.
//!
//! **The wedge this exists for (#2861).** CEF shares a popup's renderer with
//! the opener until the popup's first document starts, and a browser-level
//! CDP auto-attach client pauses that target; the opener's renderer then stops
//! answering and crashes. A host-created browser never shares the opener's
//! renderer, so the pause is harmless. The auto-attachers stay exactly as they
//! are.

use std::collections::{BTreeSet, HashMap};

use url::{Origin, Url};

/// The most one bridged message may carry. OAuth results are a few hundred
/// bytes; the bound is abuse's, and a larger message is dropped and named.
pub(crate) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// The most a handle may be. The shim mints 32 hex characters; anything
/// longer is not one of ours.
const MAX_HANDLE_BYTES: usize = 64;

/// One bridged popup: the tab pair, the opener document's handle for it, and
/// whether the opener's tab is still open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Channel {
    pub(crate) id: u64,
    pub(crate) opener: u16,
    pub(crate) popup: u16,
    pub(crate) handle: String,
    /// **Navigation does not clear this**; the opener tab ending does. A
    /// popup may still message the opener's new document, origin-checked.
    pub(crate) opener_open: bool,
}

/// Every bridge one host holds, indexed by the ids the pages and the tabs
/// speak.
#[derive(Default)]
pub(crate) struct Channels {
    next: u64,
    by_id: HashMap<u64, Channel>,
    /// The opener document's handles. **Dropped when that document goes**,
    /// so a handle cannot outlive the script that minted it.
    handles: HashMap<(u16, String), u64>,
    popups: HashMap<u16, u64>,
    openers: HashMap<u16, BTreeSet<u64>>,
}

impl Channels {
    /// The next bridge id. Never reused for the host's life.
    pub(crate) fn next_id(&mut self) -> u64 {
        self.next = self.next.saturating_add(1).max(1);
        self.next
    }

    /// Whether the opener document already minted this handle. **A page
    /// reusing one of its own handles is refused**, so a later message cannot
    /// land on an earlier popup. Only the host's admission asks, and the test
    /// build on a platform with no host would call it dead.
    #[cfg(target_os = "macos")]
    pub(crate) fn handle_taken(&self, opener: u16, handle: &str) -> bool {
        self.handles.contains_key(&(opener, handle.to_owned()))
    }

    /// Bind one admitted popup to its opener's handle.
    pub(crate) fn bind(
        &mut self,
        id: u64,
        opener: u16,
        popup: u16,
        handle: String,
    ) -> Result<(), String> {
        if handle.is_empty() || handle.len() > MAX_HANDLE_BYTES {
            return Err(format!("a bridge handle of {} bytes is not one of ours", handle.len()));
        }
        if self.handles.contains_key(&(opener, handle.clone())) {
            return Err("the opener reused a bridge handle".to_owned());
        }
        let channel = Channel { id, opener, popup, handle: handle.clone(), opener_open: true };
        self.handles.insert((opener, handle), id);
        self.popups.insert(popup, id);
        self.openers.entry(opener).or_default().insert(id);
        self.by_id.insert(id, channel);
        Ok(())
    }

    pub(crate) fn by_id(&self, id: u64) -> Option<Channel> {
        self.by_id.get(&id).cloned()
    }

    /// The channel an opener-side send names, by the tab that minted it and
    /// its handle.
    pub(crate) fn by_handle(&self, opener: u16, handle: &str) -> Option<Channel> {
        let id = self.handles.get(&(opener, handle.to_owned()))?;
        self.by_id(*id).filter(|channel| channel.opener == opener)
    }

    /// The channel a popup-side send names, by the popup tab alone.
    pub(crate) fn by_popup(&self, popup: u16) -> Option<Channel> {
        let id = self.popups.get(&popup)?;
        self.by_id(*id).filter(|channel| channel.popup == popup)
    }

    /// One popup's tab ended. Answers the channel so the opener can be told.
    pub(crate) fn popup_ended(&mut self, popup: u16) -> Option<Channel> {
        let id = self.popups.remove(&popup)?;
        let channel = self.by_id.remove(&id)?;
        self.handles.remove(&(channel.opener, channel.handle.clone()));
        if let Some(openers) = self.openers.get_mut(&channel.opener) {
            openers.remove(&id);
            if openers.is_empty() {
                self.openers.remove(&channel.opener);
            }
        }
        Some(channel)
    }

    /// One opener's tab ended. Answers the channels whose popups outlive it,
    /// so each is told its opener is gone. **The popup survives**, as a
    /// platform popup does; it can still read `closed` and its messages are
    /// dropped.
    pub(crate) fn opener_ended(&mut self, opener: u16) -> Vec<Channel> {
        let ids = self.openers.remove(&opener).unwrap_or_default();
        let mut ended = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(channel) = self.by_id.get_mut(&id) else { continue };
            channel.opener_open = false;
            self.handles.remove(&(opener, channel.handle.clone()));
            ended.push(channel.clone());
        }
        ended
    }

    /// The opener document navigated: its handles die with it, and the
    /// channels stay for the popup's side.
    pub(crate) fn opener_navigated(&mut self, opener: u16) {
        self.handles.retain(|(tab, _), _| *tab != opener);
    }

    /// Only a test reads this.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// One URL's origin, as the relay reports it. An opaque origin (a `data:`
/// document, an `about:blank`) reads `"null"`, the platform's own word.
pub(crate) fn origin(url: &str) -> Option<String> {
    Some(Url::parse(url).ok()?.origin().ascii_serialization())
}

/// Whether a target is a cross-origin web address. **Only `http(s)` targets
/// are bridged**: every other scheme keeps the engine's own popup path.
pub(crate) fn is_cross_origin(target: &str, opener: &str) -> bool {
    let Ok(target) = Url::parse(target) else { return false };
    if !matches!(target.scheme(), "http" | "https") {
        return false;
    }
    match Url::parse(opener) {
        Ok(opener) => match (target.origin(), opener.origin()) {
            (Origin::Tuple(..), Origin::Tuple(..)) => target.origin() != opener.origin(),
            // An opener with no web origin (about:blank, a data: document)
            // cannot be compared, and the popup is cross-origin to it.
            (Origin::Tuple(..), Origin::Opaque(_)) => true,
            _ => false,
        },
        Err(_) => true,
    }
}

/// Whether a `targetOrigin` permits delivery to a document at `recipient`.
/// **`'*'` delivers because the sender asked for it**; forge never supplies
/// it. A specific origin matches only a web origin, never an opaque one.
pub(crate) fn target_allows(target: &str, recipient: &str) -> bool {
    if target == "*" {
        return true;
    }
    let Ok(target) = Url::parse(target) else { return false };
    let Ok(recipient) = Url::parse(recipient) else { return false };
    match (target.origin(), recipient.origin()) {
        (Origin::Tuple(..), Origin::Tuple(..)) => target.origin() == recipient.origin(),
        _ => false,
    }
}

/// Whether a URL may be navigated to from a cross-origin window. The platform
/// allows a web navigation and `about:blank`; every other scheme, and the
/// other `about:` pages a page has no business steering a window to, are
/// refused.
pub(crate) fn navigable(url: &str) -> bool {
    match Url::parse(url) {
        Ok(parsed) => match parsed.scheme() {
            "http" | "https" => true,
            "about" => url == "about:blank",
            _ => false,
        },
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Channels, MAX_MESSAGE_BYTES, is_cross_origin, navigable, origin, target_allows};

    /// **A default port is the same origin**, and a different port is not:
    /// a relay that compares URLs as strings fails both ways.
    #[test]
    fn origins_are_compared_as_origins() {
        assert_eq!(origin("http://a.test:80/x"), Some("http://a.test".to_owned()));
        assert!(target_allows("http://a.test", "http://a.test:80/page"));
        assert!(!target_allows("http://a.test", "http://a.test:8080/page"));
        assert!(!target_allows("https://a.test", "http://a.test/page"));
        assert_eq!(origin("http://[::1]:9000/"), Some("http://[::1]:9000".to_owned()));
        assert_eq!(origin("about:blank"), Some("null".to_owned()));
    }

    /// **`'*'` is the sender's word, and an opaque recipient never matches a
    /// named origin.** A relay that lets `Origin::Opaque` compare equal would
    /// deliver into a `data:` document.
    #[test]
    fn a_named_target_never_matches_an_opaque_recipient() {
        assert!(target_allows("*", "about:blank"));
        assert!(!target_allows("http://a.test", "about:blank"));
        assert!(!target_allows("null", "about:blank"));
        assert!(!target_allows("not a url", "http://a.test"));
    }

    /// **Only a cross-origin web target is bridged.** A same-origin URL keeps
    /// the engine's popup path, and a non-web scheme is never a bridge's.
    #[test]
    fn only_cross_origin_web_targets_are_bridged() {
        assert!(is_cross_origin("https://accounts.test/auth", "http://app.test/"));
        assert!(is_cross_origin("http://app.test:8080/", "http://app.test/"));
        assert!(!is_cross_origin("http://app.test:80/", "http://app.test/"));
        assert!(!is_cross_origin("about:blank", "http://app.test/"));
        assert!(!is_cross_origin("data:text/html,x", "http://app.test/"));
        assert!(!is_cross_origin("javascript:void(0)", "http://app.test/"));
    }

    /// **A popup closing gives its channel back once**, and the opener's
    /// handle goes with it.
    #[test]
    fn a_closed_popup_frees_its_handle() {
        let mut channels = Channels::default();
        let id = channels.next_id();
        channels.bind(id, 1, 2, "h1".to_owned()).expect("a fresh handle");
        assert!(channels.by_handle(1, "h1").is_some());
        assert!(channels.by_popup(2).is_some());
        let ended = channels.popup_ended(2).expect("the popup ended");
        assert_eq!((ended.opener, ended.popup), (1, 2));
        assert!(channels.by_handle(1, "h1").is_none());
        assert!(channels.by_popup(2).is_none());
        assert!(channels.is_empty());
    }

    /// **An opener ending leaves the popup able to hear `closed` and nothing
    /// else**: the channel stays addressable from the popup's side, and its
    /// messages are dropped by the caller's `opener_open` check.
    #[test]
    fn an_ended_opener_keeps_the_popup_side_addressable() {
        let mut channels = Channels::default();
        let id = channels.next_id();
        channels.bind(id, 1, 2, "h1".to_owned()).expect("a fresh handle");
        let ended = channels.opener_ended(1);
        assert_eq!(ended.len(), 1);
        assert!(!ended[0].opener_open);
        assert!(channels.by_handle(1, "h1").is_none(), "the dead document's handle is gone");
        assert!(channels.by_popup(2).is_some(), "the popup can still be told");
    }

    /// **A handle belongs to the document that minted it.** An opener that
    /// navigated may not use the old document's handle, and a different tab
    /// may not use it either.
    #[test]
    fn a_handle_dies_with_its_document() {
        let mut channels = Channels::default();
        let id = channels.next_id();
        channels.bind(id, 1, 2, "h1".to_owned()).expect("a fresh handle");
        channels.opener_navigated(1);
        assert!(channels.by_handle(1, "h1").is_none());
        assert!(channels.by_popup(2).is_some(), "the popup side survives the navigation");

        let id = channels.next_id();
        channels.bind(id, 3, 4, "h2".to_owned()).expect("a fresh handle");
        assert!(channels.by_handle(1, "h2").is_none(), "another tab's handle is not this tab's");
        assert!(channels.by_handle(3, "h2").is_some());
    }

    /// **A handle is minted once.** A page reusing one would bind a later
    /// popup to an earlier proxy, and a message could land on the wrong tab.
    #[test]
    fn a_reused_handle_is_refused() {
        let mut channels = Channels::default();
        let first = channels.next_id();
        channels.bind(first, 1, 2, "h1".to_owned()).expect("a fresh handle");
        let second = channels.next_id();
        assert!(channels.bind(second, 1, 3, "h1".to_owned()).is_err());
        assert!(channels.by_handle(1, "h1").is_some());
    }

    /// **Two popups from one opener bind two channels and route by their own
    /// handle**, and closing one leaves the other addressable. There is no
    /// live rig: a second bridged start while a popup is live times out CEF's
    /// browser-info response (measured locally and in CI, `has_rfh=0`), so the
    /// routing guard lives here until the engine makes a second start safe
    /// (#2869).
    #[test]
    fn two_channels_from_one_opener_route_independently() {
        let mut channels = Channels::default();
        let first = channels.next_id();
        let second = channels.next_id();
        channels.bind(first, 1, 2, "h1".to_owned()).expect("a first popup");
        channels.bind(second, 1, 3, "h2".to_owned()).expect("a second popup");
        assert_eq!(channels.by_handle(1, "h1").map(|channel| channel.popup), Some(2));
        assert_eq!(channels.by_handle(1, "h2").map(|channel| channel.popup), Some(3));
        assert_eq!(channels.by_popup(2).map(|channel| channel.handle), Some("h1".to_owned()));
        assert_eq!(channels.by_popup(3).map(|channel| channel.handle), Some("h2".to_owned()));

        let ended = channels.popup_ended(2).expect("the first popup ended");
        assert_eq!(ended.popup, 2);
        assert!(channels.by_handle(1, "h1").is_none(), "the first handle goes with it");
        assert!(channels.by_handle(1, "h2").is_some(), "the second channel is untouched");
        assert!(channels.by_popup(3).is_some());
        assert_eq!(channels.opener_ended(1).len(), 1, "one popup outlives the opener");
    }

    /// **A handle is a small hex token, not a URL or a page's text.**
    #[test]
    fn a_handle_is_bounded() {
        let mut channels = Channels::default();
        let id = channels.next_id();
        assert!(channels.bind(id, 1, 2, String::new()).is_err());
        assert!(channels.bind(id, 1, 3, "x".repeat(65)).is_err());
        assert!(channels.bind(id, 1, 4, "x".repeat(32)).is_ok());
    }

    /// **A window may be navigated to a web page or `about:blank`**, and not
    /// to a script URL or another `about:` page from a cross-origin proxy.
    #[test]
    fn navigation_is_web_only() {
        assert!(navigable("https://accounts.test/"));
        assert!(navigable("about:blank"));
        assert!(!navigable("about:config"));
        assert!(!navigable("about:srcdoc"));
        assert!(!navigable("javascript:alert(1)"));
        assert!(!navigable("file:///etc/passwd"));
        assert!(!navigable("not a url"));
    }

    /// The payload bound is the abuse bound, not a prompt's.
    #[test]
    fn the_payload_bound_is_a_megabyte() {
        assert_eq!(MAX_MESSAGE_BYTES, 1024 * 1024);
    }
}
