//! The Playwright driver's tab list, as the service reads it.
//!
//! **Upstream's list is positional and renumbers on every close**; forge
//! needs a stable name per tab, and the patched driver appends one line with
//! each tab's CDP target id. The target id is the identity: it survives a
//! renumber, it is what the host's frames and the CDP input target already
//! speak, and the service maps it to its own small tab number.

use serde::Deserialize;

/// One tab as the driver lists it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DriverTab {
    #[serde(rename = "i")]
    pub index: usize,
    #[serde(rename = "id")]
    pub target: String,
    pub title: String,
    pub url: String,
    pub current: bool,
}

const EXPORT: &str = "FORGE-TABS ";

/// The export line's rows, or why the listing cannot be trusted.
pub fn parse_export(text: &str) -> Result<Vec<DriverTab>, String> {
    let line = text.lines().find(|line| line.starts_with(EXPORT)).ok_or_else(|| {
        "the driver's tab list carried no FORGE-TABS export, so no tab can be named".to_owned()
    })?;
    serde_json::from_str(&line[EXPORT.len()..])
        .map_err(|why| format!("the driver's tab export did not parse: {why}"))
}

/// The listing with the export line removed.
pub fn strip_export(text: &str) -> String {
    let mut listing = String::new();
    for line in text.lines().filter(|line| !line.starts_with(EXPORT)) {
        listing.push_str(line);
        listing.push('\n');
    }
    listing
}

/// The Playwright index of the tab a target id names.
pub fn index_of(tabs: &[DriverTab], target: &str) -> Option<usize> {
    tabs.iter().find(|tab| tab.target == target).map(|tab| tab.index)
}

#[cfg(test)]
mod tests {
    use super::{parse_export, strip_export};

    /// **The export is Playwright's own line plus one machine line.** The
    /// agent never sees it: the service re-renders the list in forge's words.
    #[test]
    fn the_export_names_each_tab_target_and_position() {
        let text = "FORGE-TABS [{\"i\":0,\"id\":\"ABC\",\"title\":\"one\",\"url\":\"http://a/\",\
                    \"current\":true},{\"i\":1,\"id\":\"DEF\",\"title\":\"two\",\"url\":\"http://b/\",\
                    \"current\":false}]\n- 0: (current) [one](http://a/)\n- 1: [two](http://b/)\n";
        let tabs = parse_export(text).expect("the export parses");
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1].index, 1);
        assert_eq!(tabs[1].target, "DEF");
        assert_eq!(tabs[1].title, "two");
        assert!(tabs[0].current, "the first tab is the current one");
        assert!(
            parse_export("- 0: [one](http://a/)\n").is_err(),
            "a listing with no export is refused"
        );
    }

    /// **The line and its raw text do not both reach the agent.** The caller
    /// drops the export before the parts are returned.
    #[test]
    fn the_export_is_stripped_from_the_agent_visible_text() {
        let text = "FORGE-TABS []\n- 0: (current) [one](http://a/)\n";
        assert_eq!(strip_export(text), "- 0: (current) [one](http://a/)\n");
    }
}
