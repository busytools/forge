//! The page's downloads, saved by the person's own client.
//!
//! **The bytes come to rest on the server first.** CEF downloads into the
//! profile's own `downloads/` directory - the host is a windowless process
//! with no chooser - and the completed file is announced on the control pipe.
//! The server reads it, bounded, and relays it to the controlling viewer,
//! whose shell opens the platform's save door and writes it there. A save the
//! person dismisses is silent; a file past the bound is refused out loud.

#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]

use std::path::{Path, PathBuf};

use cef::rc::Rc as _;
use cef::{
    BeforeDownloadCallback, CefString, DownloadHandler, DownloadItem,
    ImplBeforeDownloadCallback as _, ImplDownloadHandler, ImplDownloadItem as _,
    WrapDownloadHandler,
};

use crate::control::{self, HostEvent};

cef::wrap_download_handler! {
    pub struct SaveDownloadHandler {
        // The profile whose `downloads/` the file lands in.
        profile: PathBuf,
    }

    impl DownloadHandler {
        /// **`1` allows the download; the Rust trait's default refuses it.**
        /// CEF asks this before a download starts, and the binding's default
        /// is `0`, so a handler that omits it never sees one.
        fn can_download(
            &self,
            _browser: Option<&mut cef::Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> i32 {
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut cef::Browser>,
            item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            let id = item.as_ref().map_or(0, |item| item.id());
            let name = suggested_name.map_or_else(|| "download".to_owned(), ToString::to_string);
            let dir = self.profile.join("downloads");
            if std::fs::create_dir_all(&dir).is_err() {
                return 0;
            }
            let path = dir.join(format!("{id}-{}", safe_name(&name)));
            let path = CefString::from(path.to_string_lossy().as_ref());
            // **No dialog of CEF's own**: the save door is the client's,
            // after the server has the bytes.
            callback.cont(Some(&path), 0);
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut cef::Browser>,
            item: Option<&mut DownloadItem>,
            _callback: Option<&mut cef::DownloadItemCallback>,
        ) {
            let Some(item) = item else { return };
            if item.is_complete() != 1 || item.is_canceled() == 1 || item.is_interrupted() == 1 {
                return;
            }
            let path = CefString::from(&item.full_path()).to_string();
            if path.is_empty() {
                return;
            }
            let name = CefString::from(&item.suggested_file_name()).to_string();
            let size = u64::try_from(item.received_bytes()).unwrap_or(0);
            control::emit(&HostEvent::Download { name, path, size });
        }
    }
}

/// One suggested name, flattened to a file name the profile can hold.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(
            |ch| if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') { ch } else { '_' },
        )
        .take(120)
        .collect();
    let cleaned = cleaned.trim_matches('.').to_owned();
    if cleaned.is_empty() { "download".to_owned() } else { cleaned }
}

/// The handler the host's client hands CEF.
pub fn handler(profile: &Path) -> DownloadHandler {
    SaveDownloadHandler::new(profile.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    /// **A page's file name is a name, not a path.** The download lands
    /// under the profile's own directory whatever the page suggested.
    #[test]
    fn a_suggested_name_is_flattened_to_a_file_name() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name(""), "download");
        assert_eq!(safe_name(".."), "download");
    }
}
