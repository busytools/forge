//! The host's control lines to the server, on stdout.
//!
//! **The input pipe runs the other way.** The server writes pointer
//! gestures on the host's stdin; the host writes the things a viewer has to
//! answer here - a page's file chooser today - as one JSON line per event on
//! its stdout, which the server reads beside the child's frames. The log
//! stays on stderr, so neither stream is parsed for the other's content.

use std::io::Write as _;

use serde::{Deserialize, Serialize};

/// One thing the host needs the server to route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEvent {
    /// A tab the host created: the page the server asked for, or the popup a
    /// page opened under `opener`.
    TabCreated {
        tab: u16,
        opener: Option<u16>,
        url: String,
        /// The server command's id, when one asked for this tab; a popup
        /// carries none.
        #[serde(default)]
        request: Option<u64>,
    },
    /// A tab the host closed.
    TabClosed { tab: u16 },
    /// A page's file chooser: the client's picker answers with the files'
    /// bytes, and the host is handed server-local paths.
    FileDialog { id: u64, tab: u16, multiple: bool },
    /// A page's JavaScript dialog. **The host answers every one**: this line
    /// says it was raised, and a second line with `dismissed` says the
    /// deadline ran out before its agent answered.
    JsDialog { tab: u16, dialog: String, message: String, dismissed: bool },
    /// A download finished on the server: the tab it started from, the file's
    /// name, its path beside the profile, and how many bytes it holds.
    Download { tab: u16, name: String, path: String, size: u64 },
    /// A recording answered: `started` says whether the writer opened, and a
    /// later event with it false carries how the take ended.
    Recording { started: bool, seconds: u64, bytes: u64, capped: bool, error: Option<String> },
}

/// Write one event line for the server.
pub fn emit(event: &HostEvent) {
    let Ok(line) = serde_json::to_string(event) else { return };
    let mut out = std::io::stdout().lock();
    // **A server that stopped reading is a broken pipe, not a panic.** The
    // host keeps painting its ring for whoever is watching; the event is
    // lost with the connection and the dialog is answered by its timeout.
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The event one line names, or `None` for a line the server cannot read.
pub fn parse(line: &str) -> Option<HostEvent> {
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The line and the reader agree.** A chooser's request carries the
    /// dialog id the answer comes back under, or the client's files cannot
    /// be matched to the dialog that asked.
    #[test]
    fn a_file_dialog_round_trips_its_line() {
        let event = HostEvent::FileDialog { id: 7, tab: 2, multiple: true };
        let line = serde_json::to_string(&event).expect("a line");
        assert_eq!(parse(&line), Some(event));
        assert!(parse("").is_none());
        assert!(parse("{\"kind\":\"other\"}").is_none());
    }

    /// **A dialog's raise and its dismissal are two lines of one shape**, so a
    /// server can tell an answered dialog from one the deadline closed.
    #[test]
    fn a_js_dialog_round_trips_its_line() {
        let shown = HostEvent::JsDialog {
            tab: 2,
            dialog: "confirm".to_owned(),
            message: "are you sure?".to_owned(),
            dismissed: false,
        };
        let line = serde_json::to_string(&shown).expect("a line");
        assert!(line.contains("js_dialog"), "the server matches the kind: {line}");
        assert_eq!(parse(&line), Some(shown));

        let dismissed = HostEvent::JsDialog {
            tab: 2,
            dialog: "confirm".to_owned(),
            message: "are you sure?".to_owned(),
            dismissed: true,
        };
        assert_eq!(parse(&serde_json::to_string(&dismissed).expect("a line")), Some(dismissed));
    }

    /// **A tab's birth and end are named on the control line**, with the tab
    /// number and, for a popup, the tab that opened it: the server files a
    /// tab under its opener's session from this line alone.
    #[test]
    fn a_tab_created_and_closed_round_trip_their_lines() {
        let created = HostEvent::TabCreated {
            tab: 2,
            opener: Some(1),
            url: "https://example.com/".to_owned(),
            request: Some(9),
        };
        let line = serde_json::to_string(&created).expect("a line");
        assert!(line.contains("tab_created"), "the server matches the kind: {line}");
        assert_eq!(parse(&line), Some(created));

        let popup = HostEvent::TabCreated {
            tab: 3,
            opener: None,
            url: "about:blank".to_owned(),
            request: None,
        };
        let line = serde_json::to_string(&popup).expect("a line");
        assert_eq!(parse(&line), Some(popup), "a requested tab has no opener");

        let closed = HostEvent::TabClosed { tab: 2 };
        let line = serde_json::to_string(&closed).expect("a line");
        assert_eq!(parse(&line), Some(closed));
    }
}
