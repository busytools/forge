//! Cron schedule math: expression parsing, next-fire, due-check, and
//! boot catch-up.
//!
//! `chrono` is confined to THIS module. Every public function takes and
//! returns `SystemTime`, so `CronEntry`, the scheduler, and the rest of
//! forge never touch chrono - the SystemTime <-> chrono conversion
//! happens only inside [`next_fire_after`]'s private timezone helper.
//!
//! Timezone: recurring expressions evaluate in the host's LOCAL time
//! (standard cron / crontab semantics). The public [`next_fire_after`]
//! hard-codes `Local`; the generic `_in_tz` helper it delegates to lets
//! tests pin a fixed offset and stay deterministic (CI runs UTC).
//! `due_crons` and `reconcile_on_boot` need no TZ - a `SystemTime` is an
//! absolute instant, so the due-check and the catch-up decision are
//! TZ-independent.

use std::time::SystemTime;

use chrono::{DateTime, Local, TimeZone, Utc};
use croner::Cron;
use forge_primitives::cron::{CronEntry, CronId, CronKind};

/// Validate a 5-field cron expression, returning a human-readable error
/// on failure. `cron__create` calls this to reject a malformed schedule
/// at registration time with a clear message.
pub(crate) fn validate_cron_expr(expr: &str) -> Result<(), String> {
    Cron::new(expr).parse().map(|_| ()).map_err(|e| e.to_string())
}

/// The next fire for a cron `kind` strictly after `after`, evaluated in
/// the host's LOCAL timezone. `None` for a run-once whose instant has
/// passed, or a recurring expression that fails to parse / has no
/// upcoming occurrence.
pub(crate) fn next_fire_after(kind: &CronKind, after: SystemTime) -> Option<SystemTime> {
    next_fire_after_in_tz(kind, after, &Local)
}

/// TZ-generic core of [`next_fire_after`]. Split out so tests inject a
/// fixed offset (deterministic regardless of the host TZ) while prod
/// passes `Local`.
fn next_fire_after_in_tz<Tz: TimeZone>(
    kind: &CronKind,
    after: SystemTime,
    tz: &Tz,
) -> Option<SystemTime> {
    match kind {
        CronKind::Once(at) => (*at > after).then_some(*at),
        CronKind::Recurring(expr) => next_recurring_fire_in_tz(expr, after, tz),
    }
}

/// Next occurrence of cron `expr` strictly after `after`, in `tz`. The
/// sole SystemTime <-> chrono boundary: `after` converts in, the result
/// converts back out, and chrono never escapes.
fn next_recurring_fire_in_tz<Tz: TimeZone>(
    expr: &str,
    after: SystemTime,
    tz: &Tz,
) -> Option<SystemTime> {
    let cron = Cron::new(expr).parse().ok()?;
    let after_dt: DateTime<Tz> = DateTime::<Utc>::from(after).with_timezone(tz);
    let next = cron.find_next_occurrence(&after_dt, false).ok()?;
    Some(SystemTime::from(next))
}

/// The crons whose `next_fire` is at or before `now` - the scheduler's
/// per-tick due set, in input order.
pub(crate) fn due_crons(crons: &[CronEntry], now: SystemTime) -> Vec<CronId> {
    crons.iter().filter(|c| c.next_fire <= now).map(|c| c.id.clone()).collect()
}

/// Outcome of reconciling one cron against the wall clock at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootReconcile {
    /// Fire this cron once on boot - it came due while forge was down.
    pub should_fire_once: bool,
    /// The cron's next fire after reconciliation. `None` means remove the
    /// entry (a run-once that has now fired). `Some` is the next scheduled
    /// instant - unchanged when not overdue, advanced to the next FUTURE
    /// slot for an overdue recurring cron.
    pub new_next_fire: Option<SystemTime>,
}

/// Decide what to do with one cron at boot - the CATCH-UP-ONCE rule.
///
/// - Not overdue (`next_fire > now`): untouched.
/// - Overdue run-once: fire once, then delete (`new_next_fire = None`).
/// - Overdue recurring: fire ONCE (catch the latest miss, NOT every
///   skipped tick - no flood), then advance to the next FUTURE slot via
///   `next_recurring_fire`. That closure is the caller's bridge to the
///   parser, so this decision stays parser-free and unit-testable.
pub(crate) fn reconcile_on_boot(
    entry: &CronEntry,
    now: SystemTime,
    next_recurring_fire: impl FnOnce(SystemTime) -> Option<SystemTime>,
) -> BootReconcile {
    if entry.next_fire > now {
        return BootReconcile { should_fire_once: false, new_next_fire: Some(entry.next_fire) };
    }
    match &entry.kind {
        CronKind::Once(_) => BootReconcile { should_fire_once: true, new_next_fire: None },
        CronKind::Recurring(_) => {
            BootReconcile { should_fire_once: true, new_next_fire: next_recurring_fire(now) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone, Utc};
    use std::time::Duration;

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn at_utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> SystemTime {
        SystemTime::from(Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().expect("valid datetime"))
    }

    fn recurring_entry(id: &str, next_fire: u64) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: epoch(0),
            last_fire: None,
            next_fire: epoch(next_fire),
        }
    }

    fn once_entry(id: &str, at: u64) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: "forge".to_owned(),
            kind: CronKind::Once(epoch(at)),
            prompt: "p".to_owned(),
            created_at: epoch(0),
            last_fire: None,
            next_fire: epoch(at),
        }
    }

    #[test]
    fn due_crons_returns_entries_at_or_before_now_in_order() {
        let crons =
            vec![recurring_entry("past", 100), recurring_entry("exact", 200), recurring_entry("future", 300)];
        let due = due_crons(&crons, epoch(200));
        assert_eq!(due, vec![CronId::from("past"), CronId::from("exact")]);
    }

    #[test]
    fn due_crons_empty_when_nothing_due() {
        let crons = vec![recurring_entry("a", 500), recurring_entry("b", 600)];
        assert!(due_crons(&crons, epoch(100)).is_empty());
    }

    #[test]
    fn reconcile_leaves_future_entry_untouched() {
        let entry = recurring_entry("r", 500);
        let out =
            reconcile_on_boot(&entry, epoch(100), |_| panic!("a future entry must not recompute"));
        assert!(!out.should_fire_once);
        assert_eq!(out.new_next_fire, Some(epoch(500)), "next_fire unchanged");
    }

    #[test]
    fn reconcile_overdue_recurring_fires_once_and_advances_to_future_slot() {
        let entry = recurring_entry("r", 100);
        let out = reconcile_on_boot(&entry, epoch(500), |after| {
            assert_eq!(after, epoch(500), "advance is computed strictly from now, not the missed slot");
            Some(epoch(600))
        });
        assert!(out.should_fire_once, "an overdue recurring cron fires once on boot");
        assert_eq!(
            out.new_next_fire,
            Some(epoch(600)),
            "advances to the next FUTURE slot, not every missed tick",
        );
    }

    #[test]
    fn reconcile_overdue_run_once_fires_once_then_deletes() {
        let entry = once_entry("o", 100);
        let out =
            reconcile_on_boot(&entry, epoch(500), |_| panic!("a run-once never recomputes a next slot"));
        assert!(out.should_fire_once);
        assert_eq!(out.new_next_fire, None, "a fired run-once is removed");
    }

    #[test]
    fn reconcile_exact_now_is_due() {
        // next_fire == now counts as overdue (>= boundary), so it fires.
        let entry = once_entry("o", 200);
        let out = reconcile_on_boot(&entry, epoch(200), |_| None);
        assert!(out.should_fire_once, "next_fire == now is due");
    }

    #[test]
    fn validate_accepts_five_field_and_rejects_garbage() {
        assert!(validate_cron_expr("0 9 * * *").is_ok());
        assert!(validate_cron_expr("*/15 * * * *").is_ok());
        assert!(validate_cron_expr("not a cron").is_err());
        assert!(validate_cron_expr("99 99 * * *").is_err());
    }

    #[test]
    fn next_recurring_daily_slot_in_utc() {
        // 03:30 daily; from 2024-01-01T00:00Z the next slot is 03:30Z.
        let next = next_recurring_fire_in_tz("30 3 * * *", at_utc(2024, 1, 1, 0, 0), &Utc)
            .expect("has next");
        assert_eq!(next, at_utc(2024, 1, 1, 3, 30));
    }

    #[test]
    fn next_recurring_respects_injected_timezone() {
        // "0 9 * * *" is 09:00 IST; IST is +5:30, so 09:00 IST == 03:30 UTC.
        let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("valid offset");
        let next = next_recurring_fire_in_tz("0 9 * * *", at_utc(2024, 1, 1, 0, 0), &ist)
            .expect("has next");
        assert_eq!(next, at_utc(2024, 1, 1, 3, 30), "09:00 IST resolves to 03:30 UTC");
    }

    #[test]
    fn next_fire_after_once_is_future_instant_then_none_once_past() {
        let once_at = at_utc(2024, 1, 1, 12, 0);
        let kind = CronKind::Once(once_at);
        assert_eq!(next_fire_after_in_tz(&kind, at_utc(2024, 1, 1, 11, 0), &Utc), Some(once_at));
        assert_eq!(next_fire_after_in_tz(&kind, at_utc(2024, 1, 1, 13, 0), &Utc), None);
    }

    #[test]
    fn next_fire_after_recurring_dispatches_to_parser() {
        let kind = CronKind::Recurring("30 3 * * *".to_owned());
        let next =
            next_fire_after_in_tz(&kind, at_utc(2024, 1, 1, 0, 0), &Utc).expect("has next");
        assert_eq!(next, at_utc(2024, 1, 1, 3, 30));
    }

    #[test]
    fn next_fire_after_recurring_invalid_expr_is_none() {
        let kind = CronKind::Recurring("not a cron".to_owned());
        assert_eq!(next_fire_after_in_tz(&kind, at_utc(2024, 1, 1, 0, 0), &Utc), None);
    }

    #[test]
    fn next_fire_after_local_wrapper_yields_a_future_instant() {
        // The public Local wrapper: the exact instant is host-TZ
        // dependent, so only the monotonic property is asserted.
        let now = SystemTime::now();
        let next = next_fire_after(&CronKind::Recurring("*/5 * * * *".to_owned()), now)
            .expect("an every-5-minutes cron always has a next slot");
        assert!(next > now, "the next fire is in the future");
    }
}
