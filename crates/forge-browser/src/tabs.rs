//! The Playwright driver's tab list, as the service reads it.
//!
//! **The CDP target id is the one name a page has that the browser and the
//! driver agree on.** Upstream's list is positional and renumbers on every
//! close, so the service keys its own small tab numbers by the page target
//! and drives and selects by it.

use std::collections::{BTreeMap, HashMap, HashSet};

use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserTabRow, BrowserTabState};
use serde::Deserialize;

/// One tab as the driver lists it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DriverTab {
    #[serde(rename = "i")]
    pub index: usize,
    /// The page's CDP target id.
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

/// The pages the driver lists that no tab claims. **A page the driver made
/// for itself is not a host tab**, and it is closed by its target so the
/// listing cannot grow pages nothing paints.
pub fn orphan_targets(tabs: &TabRegistry, listing: &[DriverTab]) -> Vec<String> {
    let known: HashSet<String> = tabs
        .numbers()
        .into_iter()
        .filter_map(|number| tabs.target(number).map(str::to_owned))
        .collect();
    listing
        .iter()
        .filter(|row| !row.target.is_empty() && !known.contains(&row.target))
        .map(|row| row.target.clone())
        .collect()
}

/// The profile's tabs, keyed by the host's own tab number. **The registry
/// never mints one**: the host's numbers are what frames, mouse input and
/// `TabClosed` already carry, and a second counter would drift from them.
#[derive(Default)]
pub struct TabRegistry {
    tabs: BTreeMap<u16, Tab>,
    current: HashMap<SessionSlot, u16>,
    /// Every number this registry has ever taken. **A retired number is
    /// never handed out again**, so a hand-off written against it can never
    /// come to mean another tab.
    retired: HashSet<u16>,
    /// Tabs whose page a listing could not name. **Their pages are not
    /// orphans**: the prune closes nothing while one is unresolved, or a live
    /// document would be closed because its row could not be paired.
    unresolved: HashSet<u16>,
    /// Numbers the host has reported ended, including ones no row ever
    /// took. **A birth for one of these is not waiting for a page**: a popup
    /// the host gave up closed before its page existed, and a birth kept for
    /// it would be re-deferred for ever - a driver recovery on every pass.
    closed: HashSet<u16>,
}

pub(crate) struct Tab {
    pub(crate) owner: SessionSlot,
    /// The tab whose page opened this one; `None` for a tab a session owns
    /// outright. A popup closing hands its session back here.
    opener: Option<u16>,
    /// The page's CDP target id: the name the browser and the driver share.
    pub(crate) target: String,
    title: String,
    url: String,
    busy: bool,
}

impl TabRegistry {
    /// Take the host's tab `number` for `owner`. A number the registry has
    /// seen before is a refusal: the host never reuses one, so a duplicate
    /// is a bug in the pairing, not a tab to overwrite.
    pub fn adopt(
        &mut self,
        number: u16,
        owner: SessionSlot,
        target: &str,
        title: &str,
        url: &str,
    ) -> Result<u16, String> {
        if self.tabs.contains_key(&number) {
            return Err(format!("tab {number} is already open"));
        }
        if self.retired.contains(&number) {
            return Err(format!("tab {number} was already used by this host"));
        }
        self.retired.insert(number);
        self.current.insert(owner.clone(), number);
        self.tabs.insert(
            number,
            Tab {
                owner,
                opener: None,
                target: target.to_owned(),
                title: title.to_owned(),
                url: url.to_owned(),
                busy: false,
            },
        );
        Ok(number)
    }

    /// A tab the page opened: it belongs to whoever owns `opener`.
    pub fn create_under(
        &mut self,
        opener: u16,
        number: u16,
        target: &str,
        title: &str,
        url: &str,
    ) -> Result<u16, String> {
        let owner = self
            .owner(opener)
            .cloned()
            .ok_or_else(|| format!("the popup's opener tab {opener} is not open"))?;
        let adopted = self.adopt(number, owner, target, title, url)?;
        if let Some(tab) = self.tabs.get_mut(&adopted) {
            tab.opener = Some(opener);
        }
        Ok(adopted)
    }

    /// Remove one tab and answer the session that owned it. **The unresolved
    /// flag goes even for a row already gone**: a close landing between a
    /// listing and its mark must not leave the flag on a retired number.
    ///
    /// **A popup closing hands its session back to the opener.** An OAuth
    /// popup closes itself the moment it is done, and a session left with no
    /// tab would open a fresh blank page on its next call instead of the page
    /// the sign-in started from.
    pub fn close(&mut self, tab: u16) -> Option<SessionSlot> {
        self.unresolved.remove(&tab);
        // **The number is ended whether or not a row took it**, so a birth
        // still waiting for this tab's page is dropped rather than deferred.
        self.closed.insert(tab);
        let removed = self.tabs.remove(&tab)?;
        // **A popup opened by a popup keeps its chain.** The row that named
        // this tab as its opener takes this tab's own opener, so a middle
        // popup closing leaves the leaf pointing at the page still there
        // rather than at a number nobody holds.
        for other in self.tabs.values_mut() {
            if other.opener == Some(tab) {
                other.opener = removed.opener;
            }
        }
        let opener = removed.opener.filter(|opener| self.tabs.contains_key(opener));
        match opener {
            Some(opener) => {
                for number in self.current.values_mut() {
                    if *number == tab {
                        *number = opener;
                    }
                }
            }
            None => self.current.retain(|_, number| *number != tab),
        }
        Some(removed.owner)
    }

    /// Remember that a listing could not name this tab's page, so no page is
    /// pruned as an orphan while the row is unresolved. **Only an open row is
    /// marked**: a number with no row has no page to protect.
    pub fn mark_unresolved(&mut self, tab: u16) {
        if self.tabs.contains_key(&tab) {
            self.unresolved.insert(tab);
        }
    }

    /// The tabs whose page a listing could not name.
    pub fn unresolved_tabs(&self) -> Vec<u16> {
        self.unresolved.iter().copied().collect()
    }

    /// Whether the host has reported this number ended. **A birth for one is
    /// answered rather than kept**: nothing will ever list its page.
    pub fn was_closed(&self, tab: u16) -> bool {
        self.closed.contains(&tab)
    }

    pub fn close_owned(
        &mut self,
        session: &SessionSlot,
        tab: u16,
    ) -> Result<Option<SessionSlot>, String> {
        self.owned(session, tab)?;
        Ok(self.close(tab))
    }

    pub fn current(&self, session: &SessionSlot) -> Option<u16> {
        self.current.get(session).copied().filter(|tab| self.tabs.contains_key(tab))
    }

    /// Forget a session's current tab without closing it: a replacement
    /// keeps the slot but inherits none of the occupant's place.
    pub fn forget_current(&mut self, session: &SessionSlot) {
        self.current.remove(session);
    }

    pub fn select(&mut self, session: &SessionSlot, tab: u16) -> Result<(), String> {
        self.owned(session, tab)?;
        self.current.insert(session.clone(), tab);
        Ok(())
    }

    pub fn target(&self, tab: u16) -> Option<&str> {
        self.tabs.get(&tab).map(|tab| tab.target.as_str())
    }

    pub fn owner(&self, tab: u16) -> Option<&SessionSlot> {
        self.tabs.get(&tab).map(|tab| &tab.owner)
    }

    pub fn mark_busy(&mut self, tab: u16, busy: bool) {
        if let Some(entry) = self.tabs.get_mut(&tab) {
            entry.busy = busy;
        }
    }

    /// Replace a row's title and url, answering whether either changed.
    pub fn update(&mut self, tab: u16, title: &str, url: &str) -> bool {
        let Some(entry) = self.tabs.get_mut(&tab) else { return false };
        let changed = entry.title != title || entry.url != url;
        title.clone_into(&mut entry.title);
        url.clone_into(&mut entry.url);
        changed
    }

    pub(crate) fn owned(&self, session: &SessionSlot, tab: u16) -> Result<&Tab, String> {
        let entry = self.tabs.get(&tab).ok_or_else(|| format!("tab {tab} is not open"))?;
        if &entry.owner != session {
            return Err(format!("tab {tab} belongs to {}", entry.owner.display()));
        }
        Ok(entry)
    }

    /// The rows the strip draws. **A held tab outranks a busy one**: the
    /// person is being asked about it, which is the fact worth seeing. **A
    /// tab a person is driving is in use**, like one a call is driving: the
    /// spec's in-use is either.
    pub fn rows(&self, held: &HashSet<u16>, controlled: &HashSet<u16>) -> Vec<BrowserTabRow> {
        self.tabs
            .iter()
            .map(|(number, tab)| BrowserTabRow {
                number: *number,
                title: tab.title.clone(),
                url: tab.url.clone(),
                owner: tab.owner.clone(),
                state: if held.contains(number) {
                    BrowserTabState::Held
                } else if tab.busy || controlled.contains(number) {
                    BrowserTabState::InUse
                } else {
                    BrowserTabState::Idle
                },
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn numbers(&self) -> Vec<u16> {
        self.tabs.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{DriverTab, TabRegistry, orphan_targets, parse_export, strip_export};
    use forge_primitives::SessionSlot;
    use forge_primitives::browser::BrowserTabState;
    use std::collections::HashSet;

    #[test]
    fn the_hosts_number_is_the_tabs_name() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        let one = registry.adopt(1, seat.clone(), "t1", "one", "http://a/").expect("the first tab");
        let two =
            registry.adopt(2, seat.clone(), "t2", "two", "http://b/").expect("the second tab");
        assert_eq!((one, two), (1, 2));
        assert!(
            registry.adopt(2, seat.clone(), "t3", "three", "http://c/").is_err(),
            "a number is never reused",
        );
        registry.close(one);
        assert_eq!(registry.owner(two), Some(&seat), "a page keeps its number");
        assert!(
            registry.adopt(1, seat, "t4", "four", "http://d/").is_err(),
            "a closed number stays retired",
        );
    }

    #[test]
    fn only_an_owner_selects_or_closes_its_tab() {
        let lead = SessionSlot::lead("O", "p");
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let mine = registry.adopt(1, lead.clone(), "t1", "one", "u").expect("the tab");
        assert_eq!(registry.current(&lead), Some(mine), "a new tab is its owner's current");
        assert!(registry.select(&worker, mine).is_err(), "a foreign tab is refused");
        assert!(registry.close_owned(&worker, mine).is_err(), "and not closed");
        registry.select(&lead, mine).expect("its owner selects it");
        assert_eq!(registry.current(&lead), Some(mine));
        registry.close(mine);
        assert_eq!(registry.current(&lead), None, "closing the current tab clears the pointer");
    }

    #[test]
    fn rows_say_who_owns_each_tab_and_what_state_it_is_in() {
        let lead = SessionSlot::lead("O", "p");
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let held = registry.adopt(1, lead.clone(), "t1", "one", "http://a/").expect("the held tab");
        let busy =
            registry.adopt(2, worker.clone(), "t2", "two", "http://b/").expect("the busy tab");
        registry.mark_busy(busy, true);
        let rows = registry.rows(&HashSet::from([held]), &HashSet::new());
        assert_eq!(rows[0].state, BrowserTabState::Held);
        assert_eq!(rows[1].state, BrowserTabState::InUse);
        assert_eq!(rows[0].owner, lead);
        assert_eq!(rows[1].owner, worker);

        // **A person's control is a use too.** A tab nobody's call is driving
        // reads in use while a viewer holds its role.
        let driven =
            registry.adopt(3, worker.clone(), "t3", "three", "http://c/").expect("the driven tab");
        let rows = registry.rows(&HashSet::from([held]), &HashSet::from([driven]));
        assert_eq!(rows[2].state, BrowserTabState::InUse, "the controlled tab is in use");

        // A neighbour's close moves no number and leaves the held tab held.
        registry.close(busy);
        registry.close(driven);
        let rows = registry.rows(&HashSet::from([held]), &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, held, "the held tab keeps the number the person was shown");
        assert_eq!(rows[0].state, BrowserTabState::Held);
    }

    /// **A page the driver made for itself is an orphan; a host tab is
    /// not.** The orphan is closed by target, and a row whose target the
    /// driver could not read is left alone rather than closed blind.
    #[test]
    fn only_a_driver_made_page_is_an_orphan() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        registry.adopt(1, seat, "T1", "one", "http://a/").expect("the host tab");
        let listing = [
            tab_row(0, "T1"),
            tab_row(1, "T2"),
            DriverTab {
                index: 2,
                target: String::new(),
                title: String::new(),
                url: String::new(),
                current: false,
            },
        ];
        assert_eq!(orphan_targets(&registry, &listing), vec!["T2".to_owned()]);
    }

    /// **A close clears the unresolved flag, and a number with no row is
    /// never marked**: a close landing between a listing and its mark must
    /// not hold the prune on a retired number.
    #[test]
    fn an_unresolved_flag_goes_with_its_row() {
        let seat = SessionSlot::lead("O", "p");
        let mut registry = TabRegistry::default();
        let one = registry.adopt(1, seat, "T1", "one", "u").expect("the tab");
        registry.mark_unresolved(one);
        assert_eq!(registry.unresolved_tabs(), vec![one]);
        registry.close(one);
        assert!(registry.unresolved_tabs().is_empty(), "a close clears the flag");

        registry.mark_unresolved(9);
        assert!(registry.unresolved_tabs().is_empty(), "a number with no row is not marked");
        registry.mark_unresolved(one);
        assert!(registry.unresolved_tabs().is_empty(), "a retired number is never marked");
    }

    fn tab_row(index: usize, target: &str) -> DriverTab {
        DriverTab {
            index,
            target: target.to_owned(),
            title: String::new(),
            url: String::new(),
            current: false,
        }
    }

    #[test]
    fn a_popup_belongs_to_its_openers_owner() {
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let opener = registry.adopt(1, worker.clone(), "t1", "one", "u").expect("the opener");
        let popup =
            registry.create_under(opener, 2, "t2", "two", "http://b/").expect("the opener exists");
        assert_eq!(
            registry.owner(popup),
            Some(&worker),
            "the popup is the opener's, not the caller's"
        );
        assert!(
            registry.create_under(99, 3, "t3", "three", "http://c/").is_err(),
            "an unknown opener is refused",
        );
    }

    /// **A popup closing hands the session back to its opener**, and a popup
    /// outlives the opener that closed while it was open. An OAuth popup
    /// closes itself the moment it is done, and the next call must land on
    /// the page the sign-in started from rather than a fresh blank tab.
    #[test]
    fn a_popup_closing_returns_the_session_to_its_opener() {
        let worker = SessionSlot::worker("O", "p", "w");
        let mut registry = TabRegistry::default();
        let opener = registry.adopt(1, worker.clone(), "t1", "one", "u").expect("the opener");
        let popup = registry.create_under(opener, 2, "t2", "two", "http://b/").expect("the popup");
        assert_eq!(registry.current(&worker), Some(popup), "the popup is where the session is");
        registry.close(popup);
        assert_eq!(registry.current(&worker), Some(opener), "the session is back on the opener");

        let popup =
            registry.create_under(opener, 3, "t3", "three", "http://c/").expect("a second popup");
        registry.close(opener);
        assert_eq!(registry.current(&worker), Some(popup), "the popup keeps its place");
        assert_eq!(registry.owner(popup), Some(&worker), "and its owner");

        // **A chain survives its middle.** A popup opened by a popup hands the
        // session back past the one that has gone, not to a number nobody
        // holds.
        let root = registry.adopt(4, worker.clone(), "t4", "four", "u").expect("a root");
        let middle = registry.create_under(root, 5, "t5", "five", "http://e/").expect("the middle");
        let leaf = registry.create_under(middle, 6, "t6", "six", "http://f/").expect("the leaf");
        assert_eq!(registry.current(&worker), Some(leaf), "the leaf is where the session is");
        registry.close(middle);
        assert_eq!(registry.current(&worker), Some(leaf), "the middle closing leaves the leaf");
        registry.close(leaf);
        assert_eq!(
            registry.current(&worker),
            Some(root),
            "the leaf hands the session back past the middle",
        );
    }

    /// **The export is Playwright's own line plus one machine line.** The
    /// agent never sees it: the service re-renders the list in forge's words.
    #[test]
    fn the_export_names_each_tab_target_and_position() {
        let text = "FORGE-TABS [{\"i\":0,\"target\":\"T1\",\"title\":\"one\",\
                    \"url\":\"http://a/\",\"current\":true},{\"i\":1,\
                    \"target\":\"T2\",\"title\":\"two\",\"url\":\"http://b/\",\"current\":false}]\n\
                    - 0: (current) [one](http://a/)\n- 1: [two](http://b/)\n";
        let tabs = parse_export(text).expect("the export parses");
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1].index, 1);
        assert_eq!(tabs[1].target, "T2", "the target is where CDP drives it");
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
