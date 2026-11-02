//! Asks a person cannot see, decided in the shell's own half.
//!
//! **A parked page runs no timer.** A WKWebView with the display off consumes
//! its socket only while the page is awake, its timers stop with it, and a
//! frozen Android WebView or a hidden tab throttles to about a tick a minute -
//! so a notification rule living only in the page is dead exactly when a
//! notification matters. The bridge reads every frame whether or not the page
//! runs, so it decides here, over the same home roster the page reads and on
//! the same ask identity: a seat's pending kind and `pending_seq`.
//!
//! **The rule is the page's.** An ask is news when it arrives for a seat the
//! person is not looking at - not the shown seat, or the window is not
//! focused. A first roster read is the launch's own history and announces
//! nothing; only announced asks are marked, so one glanced at fires once the
//! person looks away; and an answer that shrinks a batch is not news.

use std::collections::{HashMap, HashSet};
#[cfg(mobile)]
use std::sync::atomic::{AtomicBool, Ordering};

use forge_primitives::SessionSlot;
use serde_json::Value;

/// Whether the app is in front, as the platform reports it.
///
/// **`window.is_focused()` is not that on Android**: tao's window answers
/// false there unconditionally, so the rule's "the seat on screen is not
/// news while it is looked at" could never hold on the phone. The app's own
/// resume and suspend events are the truth this side has.
#[cfg(mobile)]
static FOREGROUND: AtomicBool = AtomicBool::new(true);

/// Note whether the app came to the front or went behind.
#[cfg(mobile)]
pub(crate) fn set_foreground(foreground: bool) {
    FOREGROUND.store(foreground, Ordering::Relaxed);
}

/// Whether the app is in front, on a platform whose window cannot say.
#[cfg(target_os = "android")]
pub(crate) fn foreground() -> bool {
    FOREGROUND.load(Ordering::Relaxed)
}

/// The kind of ask a roster row reports, on the wire's own names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskKind {
    Question,
    Permission,
    HandOff,
}

impl AskKind {
    /// The ask in the OS notification's own words.
    pub fn body(self) -> &'static str {
        match self {
            Self::Question => "a question is waiting",
            Self::Permission => "a permission prompt is waiting",
            Self::HandOff => "a hand-off is waiting",
        }
    }
}

/// One seat's ask, as `AgentWire` writes it.
struct Asked {
    slot: SessionSlot,
    kind: AskKind,
    seq: u64,
}

/// One row's ask, or `None` when the row holds none.
///
/// **A row this build cannot read is skipped, not fatal.** A newer server's
/// ask kind is one row's news this build has no vocabulary for; failing the
/// array would silence every other seat's ask with it.
fn asked_of(row: &Value) -> Result<Option<Asked>, String> {
    let Some(slot) = row.get("slot") else {
        return Err("the row names no seat".to_owned());
    };
    let slot: SessionSlot = serde_json::from_value(slot.clone())
        .map_err(|why| format!("the row's seat is unreadable: {why}"))?;
    let pending = row.get("pending");
    let kind = match pending {
        None | Some(Value::Null) => return Ok(None),
        Some(held) => match held.as_str() {
            Some("question") => AskKind::Question,
            Some("permission") => AskKind::Permission,
            Some("hand_off") => AskKind::HandOff,
            other => {
                return Err(format!(
                    "{} names a pending kind this build does not know: {other:?}",
                    slot.display(),
                ));
            }
        },
    };
    let seq = row.get("pending_seq").and_then(Value::as_u64).unwrap_or(0);
    Ok(Some(Asked { slot, kind, seq }))
}

/// One decision wait that is news, as the shell raises it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitNotice {
    /// The seat that raised it.
    pub slot: SessionSlot,
    /// The task's id, which with `at` is the wait's identity.
    pub task: String,
    /// The ask's own words, verbatim.
    pub words: String,
    /// The other raises the same read carries for this seat.
    pub more: usize,
}

/// What has been announced, and whether a roster read has seeded it.
#[derive(Default)]
pub struct Asks {
    /// The server run an ask was announced in, its kind and its number.
    ///
    /// **The run scopes the number**: a forge restart starts the counter
    /// again, and a mark from a dead run must never match a live one.
    seen: HashMap<SessionSlot, (String, AskKind, u64)>,
    seeded: bool,
    /// The decision waits already announced: a seat and a task, by the
    /// moment the wait began.
    wait_seen: HashMap<(SessionSlot, String), String>,
    waits_seeded: bool,
}

impl Asks {
    /// The asks one home read raises.
    ///
    /// A roster this half cannot read announces nothing and seeds nothing: a
    /// read that did not parse is not the launch's history.
    pub fn rising(
        &mut self,
        data: &Value,
        shown: Option<&SessionSlot>,
        focused: bool,
    ) -> Vec<(SessionSlot, AskKind)> {
        let Some(rows) = data.get("agents").and_then(Value::as_array) else {
            return Vec::new();
        };
        let boot = data.get("boot_id").and_then(Value::as_str).unwrap_or_default();
        let mut agents = Vec::with_capacity(rows.len());
        for row in rows {
            match asked_of(row) {
                Ok(Some(agent)) => agents.push(agent),
                Ok(None) => {}
                Err(why) => tauri_plugin_log::log::debug!(
                    "a roster row could not be read for the ask rule: {why}",
                ),
            }
        }
        let first = !self.seeded;
        self.seeded = true;
        let mut notices = Vec::new();
        let mut live = HashSet::with_capacity(agents.len());
        for agent in &agents {
            let Asked { slot, kind, seq } = agent;
            live.insert(slot.clone());
            if self
                .seen
                .get(slot)
                .is_some_and(|(run, held, held_seq)| run == boot && held == kind && held_seq == seq)
            {
                continue;
            }
            let mark = (boot.to_owned(), *kind, *seq);
            if first {
                self.seen.insert(slot.clone(), mark);
                continue;
            }
            // **A seat on screen is not news while it is looked at**, and it
            // is not marked either: the read after the person looks away
            // announces it.
            if shown == Some(slot) && focused {
                continue;
            }
            self.seen.insert(slot.clone(), mark);
            notices.push((slot.clone(), *kind));
        }
        self.seen.retain(|slot, _| live.contains(slot));
        notices
    }

    /// The decision waits one home read raises.
    ///
    /// **A first read is the launch's own history**, exactly as the asks
    /// are, so a relaunch with waits open announces none. The identity is the
    /// task and the moment the wait began - task data, not a counter, so the
    /// server run is not part of it: a restart leaves the same open waits the
    /// same, and a wait answered and raised again is a new one.
    ///
    /// **A raise notifies whatever is on screen**: the app being frontmost
    /// does not mean the project holding the wait is being looked at. A read
    /// is grouped by seat, and a seat's raises collapse into one notice
    /// naming the first and counting the others raised with it.
    pub fn waits_rising(&mut self, data: &Value) -> Vec<WaitNotice> {
        let Some(rows) = data.get("fleet").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut waits: Vec<(SessionSlot, WaitRow)> = Vec::new();
        for row in rows {
            let Some(project) = row.get("project").and_then(Value::as_str) else {
                continue;
            };
            let Some(org) = org_of(data, project) else {
                tauri_plugin_log::log::debug!(
                    "a fleet row names a project with no org, so its waits cannot be opened: {project}",
                );
                continue;
            };
            let Some(items) = row.get("attention").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                match wait_of(item, &org, project) {
                    Ok(wait) => waits.push(wait),
                    Err(why) => tauri_plugin_log::log::debug!(
                        "a decision wait could not be read for the notify rule: {why}",
                    ),
                }
            }
        }
        let first = !self.waits_seeded;
        self.waits_seeded = true;
        let mut live = HashSet::with_capacity(waits.len());
        let mut seats: Vec<(SessionSlot, Vec<WaitRow>)> = Vec::new();
        for (slot, wait) in waits {
            live.insert((slot.clone(), wait.task.clone()));
            match seats.iter_mut().find(|(held, _)| held == &slot) {
                Some((_, group)) => group.push(wait),
                None => seats.push((slot, vec![wait])),
            }
        }
        let mut notices = Vec::new();
        for (slot, group) in seats {
            let mut fresh: Vec<&WaitRow> = Vec::new();
            for wait in &group {
                // **The wait's own moment is its identity**: task data, so a
                // server restart leaves the same open waits the same, and an
                // edit that moves the row's `updated_at` is not a new ask.
                let mark = wait.at.clone();
                if self.wait_seen.get(&(slot.clone(), wait.task.clone())) == Some(&mark) {
                    continue;
                }
                self.wait_seen.insert((slot.clone(), wait.task.clone()), mark);
                if !first {
                    fresh.push(wait);
                }
            }
            if let Some(head) = fresh.first() {
                notices.push(WaitNotice {
                    slot: slot.clone(),
                    task: head.task.clone(),
                    words: head.words.clone(),
                    more: fresh.len() - 1,
                });
            }
        }
        // An answered wait leaves no mark, so the same task raised again is
        // news.
        self.wait_seen.retain(|key, _| live.contains(key));
        notices
    }
}

/// One wait as the rule groups it: the task, the ask's words and its moment.
struct WaitRow {
    task: String,
    words: String,
    at: String,
}

/// One wait's seat and row, or the reason it cannot be read.
fn wait_of(item: &Value, org: &str, project: &str) -> Result<(SessionSlot, WaitRow), String> {
    let task = item.get("task").and_then(Value::as_str);
    let owner = item.get("owner").and_then(Value::as_str);
    let words = item.get("words").and_then(Value::as_str);
    let (Some(task), Some(owner), Some(words)) = (task, owner, words) else {
        return Err("the wait does not name its task, seat and words".to_owned());
    };
    let slot = match owner {
        "lead" => SessionSlot::lead(org, project),
        label => SessionSlot::worker(org, project, label),
    };
    // A server too old to send the moment still announces the wait; the task
    // alone is then its identity.
    let at = item
        .get("at")
        .and_then(|held| {
            let secs = held.get("secs_since_epoch").and_then(Value::as_u64)?;
            let nanos = held.get("nanos_since_epoch").and_then(Value::as_u64)?;
            Some(format!("{secs}.{nanos}"))
        })
        .unwrap_or_default();
    Ok((slot, WaitRow { task: task.to_owned(), words: words.to_owned(), at }))
}

/// The org a fleet row's project belongs to, from the same read's projects.
fn org_of(data: &Value, project: &str) -> Option<String> {
    for row in data.get("projects")?.as_array()? {
        let held = row.get("project")?;
        if held.get("name").and_then(Value::as_str) == Some(project) {
            return held.get("org").and_then(Value::as_str).map(str::to_owned);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{AskKind, Asks, WaitNotice};
    use forge_primitives::SessionSlot;
    use serde_json::json;

    fn seat(label: &str) -> serde_json::Value {
        json!({ "org": "Busytools", "project": "forge", "label": label })
    }

    /// A home read whose one project holds `waits`: task, owner, words, and
    /// the second the wait began.
    fn waiting(waits: &[(&str, &str, &str, u64)]) -> serde_json::Value {
        let items: Vec<serde_json::Value> = waits
            .iter()
            .map(|(task, owner, words, at)| {
                json!({
                    "task": task,
                    "owner": owner,
                    "subject": "a row",
                    "words": words,
                    "at": { "secs_since_epoch": at, "nanos_since_epoch": 0 },
                })
            })
            .collect();
        json!({
            "boot_id": "run-1",
            "projects": [{ "project": { "org": "Busytools", "name": "forge" } }],
            "fleet": [{ "project": "forge", "attention": items }],
        })
    }

    fn roster(rows: &[(&str, Option<&str>, u64)]) -> serde_json::Value {
        roster_of("run-1", rows)
    }

    fn roster_of(run: &str, rows: &[(&str, Option<&str>, u64)]) -> serde_json::Value {
        let agents: Vec<serde_json::Value> = rows
            .iter()
            .map(|(label, pending, seq)| {
                json!({
                    "slot": seat(label),
                    "pending": pending,
                    "pending_depth": 1,
                    "pending_seq": seq,
                })
            })
            .collect();
        json!({ "boot_id": run, "agents": agents })
    }

    /// A roster whose rows carry their own depth, for the batch cases.
    fn roster_depth(rows: &[(&str, Option<&str>, u64, usize)]) -> serde_json::Value {
        let agents: Vec<serde_json::Value> = rows
            .iter()
            .map(|(label, pending, seq, depth)| {
                json!({
                    "slot": seat(label),
                    "pending": pending,
                    "pending_depth": depth,
                    "pending_seq": seq,
                })
            })
            .collect();
        json!({ "boot_id": "run-1", "agents": agents })
    }

    fn lead() -> SessionSlot {
        SessionSlot::lead("Busytools", "forge")
    }

    fn worker() -> SessionSlot {
        SessionSlot::worker("Busytools", "forge", "w1")
    }

    /// **A first read is the launch's own history**, so a relaunch with a
    /// parked ask announces none of it.
    #[test]
    fn the_first_read_seeds_without_announcing() {
        let mut asks = Asks::default();
        let parked = roster(&[("lead", Some("permission"), 1), ("w1", Some("question"), 1)]);
        assert!(asks.rising(&parked, None, true).is_empty(), "launch is not news");
        assert!(
            asks.rising(&parked, None, true).is_empty(),
            "and the second read agrees with the seed",
        );
    }

    /// An ask on another seat than the one shown is news.
    #[test]
    fn another_seats_ask_is_news() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("question"), 2)]), Some(&lead()), true),
            vec![(worker(), AskKind::Question)],
        );
    }

    /// **The shown seat is announced once it is not looked at**, and the
    /// glance does not consume it.
    #[test]
    fn the_shown_seat_waits_for_the_person_to_look_away() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        let parked = roster(&[("lead", Some("permission"), 3)]);
        assert!(asks.rising(&parked, Some(&lead()), true).is_empty(), "looked at");
        assert!(asks.rising(&parked, Some(&lead()), true).is_empty(), "still looked at");
        assert_eq!(
            asks.rising(&parked, Some(&lead()), false),
            vec![(lead(), AskKind::Permission)],
            "the window lost focus, so it is news",
        );
    }

    /// **A re-read is not a new ask**, and an answer that shrinks the batch is
    /// not news either - the ask's sequence is the identity.
    #[test]
    fn an_answer_that_shrinks_the_batch_is_not_news() {
        let mut asks = Asks::default();
        asks.rising(&roster_depth(&[]), None, true);
        assert_eq!(
            asks.rising(&roster_depth(&[("w1", Some("permission"), 5, 3)]), None, true).len(),
            1,
            "three waiting is news",
        );
        assert!(
            asks.rising(&roster_depth(&[("w1", Some("permission"), 5, 2)]), None, true).is_empty(),
            "one answered and none arrived, so the batch that shrank is not news",
        );
    }

    /// **An ask replacing another at the same depth is news**, which a
    /// kind-and-depth identity could not see.
    #[test]
    fn a_replaced_ask_at_the_same_depth_is_news() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        asks.rising(&roster(&[("w1", Some("question"), 4)]), None, true);
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("permission"), 5)]), None, true),
            vec![(worker(), AskKind::Permission)],
        );
    }

    /// **A forge restart starts the counter again, so the run scopes the
    /// mark.** A mark from the dead run cannot swallow this run's first ask.
    #[test]
    fn a_restart_starts_the_marks_over() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("question"), 1)]), None, true),
            vec![(worker(), AskKind::Question)],
        );
        assert!(
            asks.rising(&roster(&[("w1", Some("question"), 1)]), None, true).is_empty(),
            "the same run's same number is the same ask",
        );
        assert_eq!(
            asks.rising(&roster_of("run-2", &[("w1", Some("question"), 1)]), None, true),
            vec![(worker(), AskKind::Question)],
            "the next run's number 1 is a different ask",
        );
    }

    /// **One unreadable row does not silence the rest.** A newer server's ask
    /// kind is skipped and said in the log; the other seats still announce.
    #[test]
    fn an_unknown_ask_kind_skips_its_row_and_keeps_the_others() {
        let mut asks = Asks::default();
        asks.rising(&roster(&[]), None, true);
        let rows = roster(&[("lead", Some("elicit"), 1), ("w1", Some("question"), 1)]);
        assert_eq!(
            asks.rising(&rows, None, true),
            vec![(worker(), AskKind::Question)],
            "the row this build cannot read is skipped, not the array",
        );
    }

    /// A roster this half cannot read is not the launch's history: it seeds
    /// nothing, so the first READABLE read is the one that seeds - and the
    /// ask after it is the first that can be news.
    #[test]
    fn an_unreadable_read_does_not_seed() {
        let mut asks = Asks::default();
        assert!(asks.rising(&json!({ "agents": "not a list" }), None, true).is_empty());
        assert!(
            asks.rising(&roster(&[("w1", Some("question"), 1)]), None, true).is_empty(),
            "the first readable read is the launch's history",
        );
        assert_eq!(
            asks.rising(&roster(&[("w1", Some("question"), 2)]), None, true),
            vec![(worker(), AskKind::Question)],
            "the ask after it is news",
        );
    }

    /// **A read's raises on one seat collapse to one notice**, naming the
    /// first ask and counting the others raised with it.
    #[test]
    fn a_reads_wait_raises_collapse_to_one_notice() {
        let mut asks = Asks::default();
        asks.waits_rising(&waiting(&[]));
        let two = waiting(&[("t1", "lead", "sign this off", 5), ("t2", "lead", "look at this", 9)]);
        assert_eq!(
            asks.waits_rising(&two),
            vec![WaitNotice {
                slot: lead(),
                task: "t1".to_owned(),
                words: "sign this off".to_owned(),
                more: 1,
            }],
        );
        assert!(asks.waits_rising(&two).is_empty(), "the same read is not news");
    }

    /// **The wait's moment is its identity and the server run is not**: a
    /// restart changes no task data, so the open waits it finds are the ones
    /// already announced; the same task raised again at a later moment is
    /// news.
    #[test]
    fn a_waits_moment_is_its_identity_and_the_run_is_not() {
        let mut asks = Asks::default();
        asks.waits_rising(&waiting(&[]));
        assert_eq!(
            asks.waits_rising(&waiting(&[("t1", "lead", "sign this off", 5)])).len(),
            1,
            "the first raise is news",
        );
        assert!(
            asks.waits_rising(&waiting(&[("t1", "lead", "sign this off", 5)])).is_empty(),
            "the same wait is not news",
        );
        let mut restarted = waiting(&[("t1", "lead", "sign this off", 5)]);
        restarted["boot_id"] = json!("run-2");
        assert!(
            asks.waits_rising(&restarted).is_empty(),
            "a restart re-announces nothing it already announced",
        );
        assert_eq!(
            asks.waits_rising(&waiting(&[("t1", "lead", "sign this off", 9)])).len(),
            1,
            "the same task raised again at a new moment is news",
        );
    }

    /// **An answered wait leaves no mark**, so the same task raised again is
    /// news.
    #[test]
    fn an_answered_wait_raised_again_is_news() {
        let mut asks = Asks::default();
        asks.waits_rising(&waiting(&[]));
        assert_eq!(asks.waits_rising(&waiting(&[("t1", "lead", "sign this off", 5)])).len(), 1,);
        assert!(asks.waits_rising(&waiting(&[])).is_empty(), "answered");
        assert_eq!(
            asks.waits_rising(&waiting(&[("t1", "lead", "sign this off", 5)])).len(),
            1,
            "raised again after the answer",
        );
    }
}
