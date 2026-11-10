//! The cron subsystem's host wiring: the delivery path into a session, the
//! project catalogue and worker registry the fire pass reads, the supervisor
//! that ticks it, and the persisted rows a restart reads back.
//!
//! The subsystem itself lives in `forge_tools::cron`; these tests stay here
//! because what they exercise is the host half. The workspace keeps the same
//! cron methods (they delegate to the handle), so the bodies read as they
//! did when the state lived in the workspace.

use std::sync::Arc;

use tempfile::tempdir;

use crate::SessionSlot;
use crate::crons::CRON_TICK_INTERVAL;
use crate::protocol::{Command, SessionUpdate};
use crate::target::ProjectKey;
use crate::workspace::PooledAgent;
use crate::workspace::Workspace;
use forge_gateway::AccountKey;

/// The cron prompts parked for `(project, label)`, under whichever org
/// the seeded project belongs to.
fn parked_crons(ws: &crate::Workspace, project: &str, label: Option<&str>) -> Vec<String> {
    let org =
        ws.list_projects().into_iter().find(|v| v.name == project).expect("seeded project").org;
    ws.parked_by_slot
        .lock()
        .get(&crate::SessionSlot::for_label(&org, project, label))
        .map(|parked| parked.cron.iter().map(|p| p.text.clone()).collect())
        .unwrap_or_default()
}

/// Persist a worker row for `label` in `project_key`, failing loudly
/// when it did not land. The key must resolve to a configured
/// project, since the row lands in the `(org, project name, label)`
/// keyed `sessions` table.
fn seed_worker_row(ws: &crate::Workspace, project_key: &ProjectKey, label: &str) {
    ws.seed_test_worker_row(project_key, label);
    assert!(
        ws.stored_worker_row(project_key, label).expect("read the seeded row").is_some(),
        "the worker row for {label} did not land; does {project_key:?} resolve to a project?",
    );
}

/// Seed the `proj` fixture with a project path that exists, and
/// return its key. A non-git worker runs in that directory, so the
/// wave checks it is still there before re-spawning the worker - a
/// fixture path that cannot exist is not a shape production has.
/// A workspace whose `proj` sits at a real directory, with one worker
/// row for `label` recorded as git-backed and no worktree on disk -
/// the stranded-owner shape every fire test in this module is built on.
/// Both tempdirs must outlive the caller.
fn stranded_owner_fixture(
    label: &str,
) -> (Arc<Workspace>, ProjectKey, tempfile::TempDir, tempfile::TempDir) {
    let (ws, _rx) = Workspace::testing_stub();
    let dir = tempdir().expect("tempdir");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    let project_dir = tempdir().expect("project dir");
    let key = seed_project_with_a_real_root(&ws, &project_dir);
    ws.record_worker_row(
        &key,
        label,
        &format!("{label}-uuid"),
        "c",
        None,
        None,
        false,
        crate::mcp::workers::types::WorkerTree { is_git_repo: true, cwd: None },
        None,
    )
    .expect("seed the stranded worker's row");
    (ws, key, dir, project_dir)
}

fn seed_project_with_a_real_root(ws: &crate::Workspace, dir: &tempfile::TempDir) -> ProjectKey {
    let root = dir.path().join("proj-root");
    std::fs::create_dir_all(&root).expect("create the project dir");
    ws.seed_test_project("proj", &root.to_string_lossy());
    ws.project_key_for_name("proj").expect("seeded project")
}

fn live_worker_entry(project: &str, label: &str) -> crate::mcp::workers::types::WorkerEntry {
    crate::mcp::workers::types::WorkerEntry {
        label: label.to_owned(),
        charter: "c".to_owned(),
        slot: SessionSlot::worker("TestOrg", project, label),
        session_id: None,
        status: forge_primitives::WorkerLiveness::Running,
        spawned_at: std::time::SystemTime::UNIX_EPOCH,
        spawned_by: SessionSlot::lead("TestOrg", project),
        needs_tag: false,
        is_git_repo_at_spawn: false,
        cwd: None,
        diagnostic: None,
        kick: None,
    }
}

/// Drain every currently-queued `SessionUpdate` from the test rx.
fn drain_updates(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
) -> Vec<crate::protocol::SessionUpdate> {
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        out.push(std::sync::Arc::unwrap_or_clone(frame.update));
    }
    out
}

fn project_expanded_path(workspace: &Workspace, name: &str) -> String {
    workspace.list_projects().into_iter().find(|p| p.name == name).map_or_else(
        || panic!("project '{name}' missing from workspace"),
        |p| p.path.to_string_lossy().into_owned(),
    )
}

/// One daily schedule, as a test's write leaves it.
fn schedule(
    id: &str,
    project: &str,
    prompt: &str,
    owner: Option<&str>,
) -> forge_primitives::cron::CronEntry {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    CronEntry {
        id: CronId::from(id),
        project_name: project.to_owned(),
        kind: CronKind::Recurring("0 9 * * *".to_owned()),
        prompt: prompt.to_owned(),
        created_at: std::time::SystemTime::UNIX_EPOCH,
        description: None,
        last_fire: None,
        next_fire: std::time::SystemTime::UNIX_EPOCH,
        team_role: owner.map(str::to_owned),
        wake: true,
    }
}

/// One delivery as the fire router hands it to `deliver_cron_prompt`: a
/// cron entry carrying the project, owner and prompt the delivery
/// routes and reads. The id is fixed - these tests are about the route.
fn deliver_cron(
    ws: &Arc<Workspace>,
    project: &str,
    owner: Option<&str>,
    prompt: &str,
    missed: bool,
) -> crate::spawn::CronFireOutcome {
    crate::spawn::deliver_cron_prompt(
        ws,
        &schedule("delivered-cron", project, prompt, owner),
        missed,
    )
}

/// The one `CronSchedulesChanged` on a test's update stream, as its key
/// and the set it announces.
fn next_crons_changed(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
) -> (SessionSlot, Vec<forge_primitives::cron::CronEntry>) {
    match rx.try_recv().map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
        Ok(SessionUpdate::CronSchedulesChanged { key, crons }) => (key, crons),
        other => panic!("expected a CronSchedulesChanged on the stream, got {other:?}"),
    }
}

/// A cron create and a cron delete each announce the project's whole
/// schedule set on its lead seat, so the inspector's SCHEDULES section
/// pops on the frame that moved it rather than on the next unrelated
/// read. Mutants: drop either emission (the drain is empty); announce a
/// set taken before the write.
#[test]
fn a_cron_write_announces_the_projects_schedules() {
    use forge_primitives::cron::CronId;
    let dir = tempdir().expect("tempdir");
    let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/tp-crons");
    let lead = SessionSlot::lead("TestOrg", "proj");

    ws.push_cron(schedule("c1", "proj", "stand-up", None));
    let (key, crons) = next_crons_changed(&mut rx);
    assert_eq!(key, lead, "a create routes on the project's lead seat");
    assert_eq!(
        crons.iter().map(|cron| cron.prompt.clone()).collect::<Vec<_>>(),
        ["stand-up"],
        "and carries the set the core now holds",
    );

    assert!(
        ws.remove_cron_owned_by("proj", &CronId::from("c1"), None).is_some(),
        "precondition: the lead deletes its own cron",
    );
    let (_, crons) = next_crons_changed(&mut rx);
    assert!(crons.is_empty(), "a delete announces the set it left behind");
}

/// A cron write that changed nothing announces nothing: an id no entry
/// carries, a delete refused for ownership, a fire for an id that is
/// gone, and a teardown matching no worker cron are all no-ops.
///
/// Mutant: emit unconditionally, where each of these is news the section
/// never moved for.
#[test]
fn a_cron_write_that_changed_nothing_announces_nothing() {
    use forge_primitives::cron::CronId;
    let dir = tempdir().expect("tempdir");
    let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/tp-crons-noop");
    let key = ws.project_key_for_name("proj").expect("seeded project");

    assert!(
        !ws.remove_cron("proj", &CronId::from("ghost")),
        "precondition: no entry carries the id",
    );
    assert!(
        ws.remove_cron_owned_by("proj", &CronId::from("ghost"), None).is_none(),
        "precondition: no entry carries the id for that owner either",
    );
    ws.seed_test_cron(schedule("c1", "proj", "stand-up", Some("reviewer")));
    assert!(
        ws.remove_cron_owned_by("proj", &CronId::from("c1"), None).is_none(),
        "precondition: the entry belongs to the worker, so the lead's delete is refused",
    );
    ws.delete_crons_for_worker(&key, "nobody");
    ws.advance_or_remove_cron(&CronId::from("ghost"), std::time::SystemTime::now());

    let announced = rx.try_recv();
    assert!(
        announced.is_err(),
        "no write here changed a schedule, so nothing is announced, and this was: {announced:?}",
    );
}

/// The writes no tool call reaches announce as well - a fire that
/// advances or removes an entry, the fire router's dead-slot removal, and
/// a worker teardown that takes its schedules with it - because the
/// section draws `next_fire` and the set itself, not only what the tools
/// edit.
///
/// Mutant: wire only the tool-driven writes, where a fired schedule's
/// countdown, a vanished owner's cron and a despawned worker's schedules
/// all stop moving.
#[test]
fn a_fired_schedule_and_a_teardown_announce_what_they_left() {
    use forge_primitives::cron::{CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/tp-crons-fire");
    let key = ws.project_key_for_name("proj").expect("seeded project");
    let lead = SessionSlot::lead("TestOrg", "proj");
    let now = std::time::SystemTime::now();

    ws.seed_test_cron(schedule("r", "proj", "stand-up", None));
    ws.advance_or_remove_cron(&CronId::from("r"), now);
    let (announced_key, crons) = next_crons_changed(&mut rx);
    assert_eq!(announced_key, lead, "a fire routes on the project's lead seat");
    assert_eq!(crons.len(), 1, "a fired recurring keeps its entry");
    assert!(crons[0].next_fire > now, "and the announcement carries the schedule it advanced to");
    assert_eq!(crons[0].last_fire, Some(now), "with the fire it recorded");

    // A run-once leaves the set with its fire, so the announcement is
    // what is left rather than the entry that is gone.
    ws.seed_test_cron(forge_primitives::cron::CronEntry {
        kind: CronKind::Once(now),
        ..schedule("o", "proj", "deploy", None)
    });
    ws.advance_or_remove_cron(&CronId::from("o"), now);
    let (_, crons) = next_crons_changed(&mut rx);
    assert!(
        crons.iter().all(|cron| cron.id != CronId::from("o")),
        "the fired run-once is announced as gone",
    );

    // The fire router's dead-slot removal - the cron whose owner left
    // forge.toml - takes the entry and announces the set it left, or a
    // cron the section no longer holds keeps its row.
    ws.seed_test_cron(schedule("orphan", "proj", "no owner", None));
    assert!(
        ws.remove_cron("proj", &CronId::from("orphan")),
        "precondition: the router's removal finds the entry",
    );
    let (announced_key, crons) = next_crons_changed(&mut rx);
    assert_eq!(announced_key, lead, "a dead-slot removal routes the same way");
    assert!(
        crons.iter().all(|cron| cron.id != CronId::from("orphan")),
        "and announces the set it left: {crons:?}",
    );

    // A worker's teardown announces the schedules it left behind.
    ws.seed_test_cron(schedule("w", "proj", "review", Some("reviewer")));
    ws.delete_crons_for_worker(&key, "reviewer");
    let (announced_key, crons) = next_crons_changed(&mut rx);
    assert_eq!(announced_key, lead, "a teardown routes on the project's lead seat");
    assert!(
        crons.iter().all(|cron| cron.team_role.as_deref() != Some("reviewer")),
        "and announces what the teardown left: {crons:?}",
    );
}

#[test]
fn cron_methods_push_list_remove_and_persist() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
    ws.install_db_for_test(db);
    let persisted =
        || crate::store::cron::list(ws.db.lock().as_ref().expect("db installed")).expect("list");

    let entry = CronEntry {
        id: CronId::from("c1"),
        project_name: "forge".to_owned(),
        kind: CronKind::Recurring("0 9 * * *".to_owned()),
        prompt: "stand-up".to_owned(),
        created_at: std::time::SystemTime::UNIX_EPOCH,
        description: None,
        last_fire: None,
        next_fire: std::time::SystemTime::UNIX_EPOCH,
        team_role: None,
        wake: true,
    };

    ws.push_cron(entry.clone());
    assert_eq!(ws.crons_for_project("forge"), vec![entry.clone()], "listed for its project");
    assert!(ws.crons_for_project("other").is_empty(), "scoped by project name");
    assert_eq!(persisted(), vec![entry.clone()], "push persisted to the store");

    // A different project cannot delete another project's cron.
    assert!(!ws.remove_cron("other", &entry.id), "delete is scoped to the owning project");
    assert_eq!(ws.crons_for_project("forge").len(), 1);

    assert!(ws.remove_cron("forge", &entry.id), "the owning project removes it");
    assert!(ws.crons_for_project("forge").is_empty());
    assert!(persisted().is_empty(), "removal persisted");

    assert!(!ws.remove_cron("forge", &CronId::from("c1")), "removing a gone id reports false");
}

/// The failed-boot-read state: the store holds durable crons, the
/// in-memory set is EMPTY (a `list` failure at boot warned and started
/// with none), and the db handle is live. Every cron mutation writes
/// only what it changed - a door that changed nothing writes nothing,
/// and a door that changed something writes its own row - so no write
/// after such a boot can erase the rows the failed read never loaded.
///
/// Both kinds of door are walked on purpose: a whole-set write that
/// only fires on a successful mutation would slip past a pin that ran
/// the no-op doors alone.
///
/// Mutant: have any mutation site (or the shared mutation helper) write
/// the whole in-memory set, at a door that changed nothing or one that
/// changed something, where either write takes both seeded rows with it.
#[test]
fn cron_mutations_over_a_failed_boot_read_do_not_erase_the_store() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/tp-crons-failed-load");
    let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
    // The rows the failed boot read left out of memory: in the store only.
    crate::store::cron::upsert(&db, &schedule("seed-1", "proj", "one", None)).expect("seed 1");
    crate::store::cron::upsert(&db, &schedule("seed-2", "proj", "two", None)).expect("seed 2");
    ws.install_db_for_test(db);
    let persisted =
        || crate::store::cron::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
    let seeded = |rows: &[forge_primitives::cron::CronEntry]| {
        rows.iter().filter(|c| c.id.as_str().starts_with("seed-")).count()
    };
    // The one assertion every door below makes: it wrote its own row at
    // most, never the whole in-memory set.
    let still_seeded = |door: &str| {
        assert_eq!(
            seeded(&persisted()),
            2,
            "after {door} the rows the failed read never loaded must still be there",
        );
    };
    let key = ws.project_key_for_name("proj").expect("seeded project");
    let now = std::time::SystemTime::now();

    ws.push_cron(schedule("created", "proj", "new", None));
    still_seeded("a create");

    // Doors that change nothing: a write here is news about a row that
    // never moved.
    assert!(
        !ws.remove_cron("proj", &CronId::from("seed-1")),
        "precondition: the empty in-memory set holds no such row",
    );
    still_seeded("a delete that removed nothing");

    assert!(
        ws.remove_cron_owned_by("proj", &CronId::from("seed-1"), None).is_none(),
        "precondition: the empty in-memory set holds no such row for that owner either",
    );
    still_seeded("an owner-scoped delete that matched nothing");

    ws.delete_crons_for_worker(&key, "worker");
    still_seeded("a worker teardown with no matching crons");

    ws.advance_or_remove_cron(&CronId::from("seed-2"), now);
    still_seeded("a fire-advance over an unloaded row");

    // Doors that DO change something, each on a loaded row: a whole-set
    // write gated behind the change takes the unloaded rows with it.
    assert!(
        ws.remove_cron("proj", &CronId::from("created")),
        "precondition: the loaded row is removed",
    );
    still_seeded("a delete of a loaded row");

    ws.push_cron(schedule("owned", "proj", "own", None));
    assert!(
        ws.remove_cron_owned_by("proj", &CronId::from("owned"), None).is_some(),
        "precondition: the loaded row is removed through the owner's door",
    );
    still_seeded("an owner-scoped delete of a loaded row");

    ws.push_cron(schedule("worker-cron", "proj", "w", Some("worker")));
    ws.delete_crons_for_worker(&key, "worker");
    still_seeded("a worker teardown that took a cron");

    ws.push_cron(CronEntry { kind: CronKind::Once(now), ..schedule("once", "proj", "fire", None) });
    ws.advance_or_remove_cron(&CronId::from("once"), now);
    still_seeded("a fire that removed a run-once");

    ws.push_cron(schedule("recurring", "proj", "again", None));
    ws.advance_or_remove_cron(&CronId::from("recurring"), now);
    still_seeded("a fire that advanced a recurring");

    ws.push_cron(CronEntry {
        kind: CronKind::Recurring("0 0 30 2 *".to_owned()),
        ..schedule("never", "proj", "never", None)
    });
    ws.advance_or_remove_cron(&CronId::from("never"), now);
    still_seeded("a fire that removed a never-occurring recurring");
}

#[test]
fn advance_or_remove_cron_recurring_advances_run_once_removes() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    let fired = std::time::SystemTime::now();

    ws.push_cron(CronEntry {
        id: CronId::from("r"),
        project_name: "forge".to_owned(),
        kind: CronKind::Recurring("*/5 * * * *".to_owned()),
        prompt: "p".to_owned(),
        created_at: fired,
        description: None,
        last_fire: None,
        next_fire: std::time::SystemTime::UNIX_EPOCH,
        team_role: None,
        wake: true,
    });
    ws.advance_or_remove_cron(&CronId::from("r"), fired);
    let after = ws.crons_for_project("forge");
    assert_eq!(after.len(), 1, "a recurring cron stays after firing");
    assert!(after[0].next_fire > fired, "next_fire advanced to a future slot");
    assert_eq!(after[0].last_fire, Some(fired), "last_fire recorded");

    ws.push_cron(CronEntry {
        id: CronId::from("o"),
        project_name: "forge".to_owned(),
        kind: CronKind::Once(std::time::SystemTime::UNIX_EPOCH),
        prompt: "p".to_owned(),
        created_at: fired,
        description: None,
        last_fire: None,
        next_fire: std::time::SystemTime::UNIX_EPOCH,
        team_role: None,
        wake: true,
    });
    ws.advance_or_remove_cron(&CronId::from("o"), fired);
    assert!(
        ws.crons_for_project("forge").iter().all(|c| c.id != CronId::from("o")),
        "a run-once is removed after firing",
    );

    // A missing id is a no-op, not a panic.
    ws.advance_or_remove_cron(&CronId::from("ghost"), fired);
}

#[test]
fn fire_due_crons_spawns_asleep_project_buffers_prompt_and_skips_future() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("forge", "/tmp/forge");

    let now = std::time::SystemTime::now();
    let past = std::time::SystemTime::UNIX_EPOCH;
    let far_future = now + std::time::Duration::from_secs(86_400);

    ws.push_cron(CronEntry {
        id: CronId::from("due"),
        project_name: "forge".to_owned(),
        kind: CronKind::Recurring("*/5 * * * *".to_owned()),
        prompt: "morning".to_owned(),
        created_at: past,
        description: None,
        last_fire: None,
        next_fire: past,
        team_role: None,
        wake: true,
    });
    ws.push_cron(CronEntry {
        id: CronId::from("later"),
        project_name: "forge".to_owned(),
        kind: CronKind::Recurring("*/5 * * * *".to_owned()),
        prompt: "later".to_owned(),
        created_at: past,
        description: None,
        last_fire: None,
        next_fire: far_future,
        team_role: None,
        wake: true,
    });

    ws.enable_test_dispatch_intercept();
    ws.fire_due_crons(now);
    let dispatched = ws.drain_test_dispatch_buffer();

    // The asleep project got exactly one SpawnProject - for the due
    // cron, not the future one.
    let spawns = dispatched
        .iter()
        .filter(|c| {
            matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
        })
        .count();
    assert_eq!(spawns, 1, "one spawn for the single due cron");

    // The due cron's prompt is buffered for its slot (the lead) for
    // delivery once the session reaches Connected.
    let buffered = parked_crons(&ws, "forge", None);
    assert_eq!(buffered, vec!["morning".to_owned()], "the due cron's prompt was buffered");

    // The due cron advanced past now; the future cron is untouched.
    let crons = ws.crons_for_project("forge");
    let due = crons.iter().find(|c| c.id == CronId::from("due")).expect("due present");
    assert!(due.next_fire > now, "the fired cron advanced past now");
    let later = crons.iter().find(|c| c.id == CronId::from("later")).expect("later present");
    assert_eq!(later.next_fire, far_future, "the not-yet-due cron is untouched");
}

#[test]
fn boot_catch_up_fires_overdue_once_advances_persists_and_does_not_refire() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
    ws.install_db_for_test(db);
    ws.seed_test_project("forge", "/tmp/forge-catchup");

    let now = std::time::SystemTime::now();
    let past = std::time::SystemTime::UNIX_EPOCH;
    let far_future = now + std::time::Duration::from_secs(86_400);

    // An overdue recurring + overdue run-once + a not-yet-due one, all
    // for the seeded "forge" project. Boot catch-up is the single
    // fire_due_crons call main.rs makes at startup.
    for entry in [
        CronEntry {
            id: CronId::from("rec"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "morning".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: past,
            team_role: None,
            wake: true,
        },
        CronEntry {
            id: CronId::from("once"),
            project_name: "forge".to_owned(),
            kind: CronKind::Once(past),
            prompt: "deploy".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: past,
            team_role: None,
            wake: true,
        },
        CronEntry {
            id: CronId::from("future"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "later".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: far_future,
            team_role: None,
            wake: true,
        },
    ] {
        ws.push_cron(entry);
    }
    assert_eq!(ws.crons_for_project("forge").len(), 3, "all three crons loaded");

    // Boot catch-up: the one call main.rs makes at startup.
    ws.enable_test_dispatch_intercept();
    ws.fire_due_crons(now);
    let first = ws.drain_test_dispatch_buffer();
    let spawns = first
        .iter()
        .filter(|c| {
            matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
        })
        .count();
    assert_eq!(spawns, 2, "boot fires the two overdue crons, not the future one");

    // Overdue recurring advanced past now + recorded last_fire; overdue
    // run-once removed; future untouched.
    let crons = ws.crons_for_project("forge");
    assert!(crons.iter().all(|c| c.id != CronId::from("once")), "overdue run-once removed");
    let rec = crons.iter().find(|c| c.id == CronId::from("rec")).expect("recurring present");
    assert!(rec.next_fire > now, "overdue recurring advanced past now");
    assert_eq!(rec.last_fire, Some(now), "the fired recurring recorded last_fire");
    let fut = crons.iter().find(|c| c.id == CronId::from("future")).expect("future present");
    assert_eq!(fut.next_fire, far_future, "the future cron is untouched");

    // The advance persisted to the store - this is what stops a double-fire.
    let persisted =
        crate::store::cron::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
    assert!(persisted.iter().all(|c| c.id != CronId::from("once")), "removal persisted");
    assert!(
        persisted.iter().find(|c| c.id == CronId::from("rec")).expect("rec persisted").next_fire
            > now,
        "advance persisted",
    );

    // The next tick does NOT re-fire: the advanced crons aren't due again.
    ws.fire_due_crons(now);
    let refires = ws
        .drain_test_dispatch_buffer()
        .iter()
        .filter(|c| matches!(c, crate::protocol::Command::SpawnProject { .. }))
        .count();
    assert_eq!(refires, 0, "advanced crons are not due again; no double-fire");
}

/// A cron fired into a running lead delivers the raw prompt as a plain
/// user turn AND echoes a `CronPromptAppended` so the chat shows a cron
/// block (mirrors the gotify running-target echo). Reproduce-first: the
/// echo is absent until `deliver_cron_prompt` calls
/// `push_cron_prompt_into_chat`.
#[test]
fn deliver_cron_prompt_to_running_lead_emits_cron_prompt_appended() {
    let dir = tempdir().expect("tempdir");
    let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    // The fire's schedule pairing is a store write, so this test needs
    // the durable layer the shipped workspace always has.
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    ws.seed_test_project("cronlead", "/tmp/cron-lead");
    // Seed the catalog + pool so the project has a running (open) lead:
    // list_projects derives `is_open` from pool membership.
    let cwd = project_expanded_path(&ws, "cronlead");
    ws.record_connected_session(&cwd, "lead-uuid", None);
    let lead_key = SessionSlot::lead("TestOrg", "cronlead");
    let (handle, _agent_rx) = Workspace::testing_stub_handle();
    ws.pool.lock().insert(
        lead_key.clone(),
        PooledAgent {
            handle: Arc::new(handle),
            account: AccountKey("test".to_owned()),
            permission_mode: None,
            registration: None,
            session_id: "pooled-session".to_owned(),
        },
    );

    ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
    ws.enable_test_dispatch_intercept();
    let mut fired = schedule("c1", "cronlead", "morning", None);
    fired.description = Some("Morning summary".to_owned());
    let outcome = crate::spawn::deliver_cron_prompt(&ws, &fired, false);
    assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));

    // The running lead receives the raw cron prompt as a plain user turn.
    let dispatched = ws.drain_test_dispatch_buffer();
    let wired_id = dispatched
        .iter()
        .find_map(|c| match c {
            Command::PromptUnder { key, text, uuid, .. }
                if key == &lead_key && text == "morning" =>
            {
                Some(uuid.clone())
            }
            _ => None,
        })
        .expect("the running lead receives the fired cron prompt verbatim, wired under an id");

    // AND the delivery echoes a CronPromptAppended so the chat shows a
    // block - under the SAME id the prompt runs under, which is what lets
    // a view hold the block's row while the prompt waits and pair it with
    // the page's copy later. The echo names the entry that fired: a view
    // reading only the prompt cannot say which schedule produced it, and
    // a run-once entry is gone from the store by the time the row draws.
    let echoed = drain_updates(&mut rx).into_iter().find_map(|u| match u {
        SessionUpdate::CronPromptAppended { key, text, uuid, cron_id, description }
            if key == lead_key && text == "morning" =>
        {
            Some((uuid, cron_id, description))
        }
        _ => None,
    });
    let (echoed_id, cron_id, description) =
        echoed.expect("the delivery echoes a cron block for the fired prompt");
    assert_eq!(
        echoed_id.as_str(),
        wired_id.as_str(),
        "the block and the fired prompt are one thing by id",
    );
    assert!(!wired_id.is_empty(), "and the id is real, not a blank");
    assert_eq!(cron_id, "c1", "the echo names the entry that fired");
    assert_eq!(description.as_deref(), Some("Morning summary"), "and its description");
    // **And the pairing is kept for the read half.** The frame names the
    // schedule once; a row replayed from a transcript afterwards has no
    // frame to read it from, so the fire is remembered under the prompt's
    // own id - the id the transcript row carries (#1815).
    let stored = crate::store::cron_names::for_uuids(
        ws.db.lock().as_ref().expect("db installed"),
        &[wired_id.as_str()],
    )
    .expect("read the stored pairing");
    assert_eq!(
        stored.get(wired_id.as_str()),
        Some(&forge_primitives::cron::CronName {
            cron_id: "c1".to_owned(),
            description: Some("Morning summary".to_owned()),
        }),
        "the fired prompt's schedule is readable after the frame is gone"
    );
}

#[test]
fn deliver_worker_cron_to_a_live_worker_prompts_the_worker() {
    let (ws, _rx) = Workspace::testing_stub();
    ws.seed_test_project("proj", "/tmp/wc-live");
    let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
    ws.insert_live_worker(&key, live_worker_entry("proj", "reviewer"));
    let worker_key = SessionSlot::worker("TestOrg", "proj", "reviewer");
    ws.mark_session_connected_for_test(&worker_key, "worker-uuid");

    ws.enable_test_dispatch_intercept();
    let outcome = deliver_cron(&ws, "proj", Some("reviewer"), "review the diff", false);
    assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
    let dispatched = ws.drain_test_dispatch_buffer();
    assert!(
        dispatched.iter().any(|c| matches!(
            c, Command::Prompt { key, text, .. } | Command::PromptUnder { key, text, .. }
                if key == &worker_key && text == "review the diff"
        )),
        "a live worker's cron fires straight into the worker",
    );
}

/// One captured tracing record: its level, its target and its fields.
#[derive(Clone, Default)]
struct Caught(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String, String)>>>);

#[derive(Default)]
struct CaughtFields(String);

impl tracing::field::Visit for CaughtFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = std::fmt::write(&mut self.0, format_args!("{}={value:?}", field.name()));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Caught {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = CaughtFields::default();
        event.record(&mut fields);
        self.0.lock().expect("capture").push((
            *event.metadata().level(),
            event.metadata().target().to_owned(),
            fields.0,
        ));
    }
}

/// The records a pass emitted for the cooling event at `level`, as
/// their captured fields.
fn cooling_records(caught: &Caught, level: tracing::Level) -> Vec<String> {
    caught
        .0
        .lock()
        .expect("capture")
        .iter()
        .filter(|(recorded, target, fields)| {
            *recorded == level
                && target == "forge_tools::cron"
                && fields.contains("cron_deferred_cooling")
        })
        .map(|(_, _, fields)| fields.clone())
        .collect()
}

/// Every `ServiceStatus` a pass sent, which is what a client draws.
fn service_notices(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
) -> Vec<String> {
    drain_updates(rx)
        .into_iter()
        .filter_map(|update| match update {
            SessionUpdate::ServiceStatus { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs()
}

/// Hold the fixture's one account out of the walk for `wait`, the
/// write the usage probe's cap makes.
fn cool_for(ws: &Workspace, wait: std::time::Duration) {
    cool_until(ws, unix_now() + wait.as_secs());
}

/// Hold the fixture's one account out of the walk until `reset_epoch`.
fn cool_until(ws: &Workspace, reset_epoch: u64) {
    ws.gateway.report_probe_limit(&AccountKey("acct-a".to_owned()), Some(reset_epoch));
}

/// `wait` from `now`, nudged to sit half a minute into a minute, so a
/// wall clock read for it cannot straddle a minute boundary between
/// the test's clock and the scheduler's own read.
fn reset_mid_minute(now: std::time::SystemTime, wait: std::time::Duration) -> u64 {
    let target = now
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs()
        + wait.as_secs();
    target - target % 60 + 30
}

/// The IST wall clock a notice prints for `unix_seconds`.
fn ist_clock(unix_seconds: u64) -> String {
    let at = chrono::DateTime::<chrono::Utc>::from(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(unix_seconds),
    );
    (at + chrono::TimeDelta::minutes(5 * 60 + 30)).format("%H:%M").to_string()
}

/// End the account's cooldown, so the walk finds it again.
fn clear_cooling(ws: &Workspace) {
    ws.gateway.report_probe_limit(&AccountKey("acct-a".to_owned()), Some(unix_now() - 60));
}

/// A cron deferred by a cooling walk stays due and says why once - the
/// model, the org and the reset - with the repeats at debug until the
/// 30-minute floor re-warns.
///
/// Mutants: warn on every tick (the second fire emits another record
/// and notice); drop the cooling detail from either line (it names no
/// model, org or reset); silence the repeat instead of recording it;
/// print the tick's own clock instead of the reset's; reset the
/// re-warn marker on every pass, where the floor counts from the
/// repeat rather than from the warning.
#[tokio::test]
async fn a_cooling_walk_defers_the_cron_and_warns_once_per_episode() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let (ws, _dir) = workspace_with_one_unsettled_account();
    ws.seed_test_ready_account("acct-a");
    let mut rx = ws.subscribe_answerer();
    ws.seed_test_cron(schedule("cool", "proj", "stand-up", None));
    let now = std::time::SystemTime::now();
    let reset_epoch = reset_mid_minute(now, std::time::Duration::from_secs(2 * 60 * 60));
    cool_until(&ws, reset_epoch);

    let caught = Caught::default();
    let subscriber = tracing_subscriber::registry().with(caught.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    ws.fire_due_crons(now);
    let warned = cooling_records(&caught, tracing::Level::WARN);
    assert_eq!(warned.len(), 1, "the first cooling tick warns once");
    let fields = &warned[0];
    let clock = ist_clock(reset_epoch);
    assert!(fields.contains("org=TestOrg"), "the warning names the org: {fields}");
    assert!(fields.contains("model=claude-sonnet-5"), "the warning names the model: {fields}");
    assert!(
        fields.contains(&format!("resets_at={clock} IST")),
        "the warning names when the reset lands: {fields}",
    );
    assert!(
        fields.contains("cooling_accounts=1"),
        "the warning counts the org's cooling accounts: {fields}",
    );
    let notices = service_notices(&mut rx);
    assert_eq!(notices.len(), 1, "and sends the client one notice: {notices:?}");
    let notice = &notices[0];
    assert!(notice.contains("claude-sonnet-5"), "the notice names the model: {notice}");
    assert!(notice.contains("TestOrg"), "the notice names the org: {notice}");
    assert!(
        notice.contains(&format!("{clock} IST")),
        "the notice names when the reset lands: {notice}",
    );
    assert!(
        notice.contains("1 account(s) cooling in the org"),
        "and counts the accounts the walk skipped: {notice}",
    );

    ws.fire_due_crons(now + std::time::Duration::from_secs(61));
    assert_eq!(
        cooling_records(&caught, tracing::Level::WARN).len(),
        1,
        "the repeat tick is debug, not another warning",
    );
    assert_eq!(
        cooling_records(&caught, tracing::Level::DEBUG).len(),
        1,
        "and says so at debug rather than staying silent",
    );
    assert!(service_notices(&mut rx).is_empty(), "and sends the client nothing");

    // 1860 s after the warning and 1799 s after the repeat: a floor
    // counted from the repeat would stay silent here.
    ws.fire_due_crons(now + std::time::Duration::from_secs(30 * 60 + 60));
    assert_eq!(
        cooling_records(&caught, tracing::Level::WARN).len(),
        2,
        "the 30-minute floor re-warns while it lasts",
    );

    let entry = ws
        .crons_for_project("proj")
        .into_iter()
        .find(|cron| cron.id == forge_primitives::CronId::from("cool"))
        .expect("a deferral keeps the entry");
    assert_eq!(
        entry.next_fire,
        std::time::SystemTime::UNIX_EPOCH,
        "and leaves it due rather than advancing",
    );
}

/// A cooling episode that clears drops its marker, so the next episode
/// warns again rather than reading as a repeat of the last one.
///
/// Mutant: keep the marker across a pass that did not find the cron
/// cooling, where the second episode stays silent.
#[tokio::test]
async fn a_cooling_episode_that_clears_warns_again_next_time() {
    let (ws, _dir) = workspace_with_one_unsettled_account();
    ws.seed_test_ready_account("acct-a");
    let mut rx = ws.subscribe_answerer();
    ws.seed_test_cron(schedule("cool", "proj", "stand-up", None));
    let now = std::time::SystemTime::now();

    cool_for(&ws, std::time::Duration::from_secs(2 * 60 * 60));
    ws.fire_due_crons(now);
    assert_eq!(service_notices(&mut rx).len(), 1, "the first episode warns once");

    // The cooldown ends: this pass finds the walk healthy and the fire
    // lands, its dispatch intercepted rather than spawning a child.
    ws.enable_test_dispatch_intercept();
    clear_cooling(&ws);
    ws.fire_due_crons(now + std::time::Duration::from_secs(61));
    assert!(service_notices(&mut rx).is_empty(), "a recovered walk has nothing to warn about");
    assert!(
        ws.drain_test_dispatch_buffer().iter().any(|command| matches!(
            command,
            Command::SpawnProject { project_name, .. } if project_name == "proj"
        )),
        "precondition: the recovered fire landed rather than deferring again",
    );

    // Cooling again, with the same entry due again, is a new episode.
    cool_for(&ws, std::time::Duration::from_secs(2 * 60 * 60));
    ws.with_crons_mut(|crons| {
        let entry = crons
            .iter_mut()
            .find(|cron| cron.id == forge_primitives::CronId::from("cool"))
            .expect("the recovered fire kept the recurring entry");
        entry.next_fire = now + std::time::Duration::from_secs(120);
    });
    ws.fire_due_crons(now + std::time::Duration::from_secs(120));
    assert_eq!(service_notices(&mut rx).len(), 1, "a new episode warns again");
}

#[test]
fn deliver_asleep_worker_cron_buffers_and_wakes_the_project() {
    let (ws, _rx) = Workspace::testing_stub();
    let dir = tempdir().expect("tempdir");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    // The row is what gives the slot a session while it is asleep; without it
    // the fire router collects the cron instead.
    let key = seed_project_with_a_real_root(&ws, &dir);
    seed_worker_row(&ws, &key, "reviewer");

    ws.enable_test_dispatch_intercept();
    let outcome = deliver_cron(&ws, "proj", Some("reviewer"), "nightly", false);
    assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
    let dispatched = ws.drain_test_dispatch_buffer();
    assert!(
        dispatched.iter().any(|c| matches!(
            c, Command::SpawnProject { project_name, .. } if project_name == "proj"
        )),
        "an asleep worker cron wakes the whole project via SpawnProject",
    );
    let buffered = parked_crons(&ws, "proj", Some("reviewer"));
    assert_eq!(buffered, vec!["nightly".to_owned()], "buffered for the worker owner");
}

/// A cron for a worker that's a live entry but still Spawning
/// (session_id None) must NOT dispatch a bare Command::Prompt
/// (dropped) - the gate in `live_cron_slot` routes it to the
/// owner-keyed buffer, drained on the worker's own Connected.
#[test]
fn deliver_cron_to_spawning_worker_buffers_via_owner() {
    let (ws, _rx) = Workspace::testing_stub();
    let dir = tempdir().expect("tempdir");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    let key = seed_project_with_a_real_root(&ws, &dir);
    seed_worker_row(&ws, &key, "reviewer");
    let worker_key = SessionSlot::worker("TestOrg", "proj", "reviewer");
    ws.insert_live_worker(&key, live_worker_entry("proj", "reviewer"));
    // Registered but not connected: session_id stays None.
    ws.register_domain_session(worker_key.clone(), None);

    ws.enable_test_dispatch_intercept();
    let outcome = deliver_cron(&ws, "proj", Some("reviewer"), "nightly", false);
    assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
    let dispatched = ws.drain_test_dispatch_buffer();
    assert!(
        !dispatched.iter().any(|c| matches!(
            c, Command::Prompt { key, .. } | Command::PromptUnder { key, .. }
                if key == &worker_key
        )),
        "no bare Prompt to the still-spawning worker (would be dropped)",
    );
    let buffered = parked_crons(&ws, "proj", Some("reviewer"));
    assert_eq!(buffered, vec!["nightly".to_owned()], "buffered for the worker's Connected drain");
}

/// A worker spawned this second is live before it connects, and a git
/// worker's worktree is created during that same window. Judged by its
/// row alone it looks unwakeable, and the fire would be dropped; it is
/// only not there YET, so the prompt parks for its own Connected drain.
#[test]
fn deliver_worker_cron_parks_for_an_owner_that_is_still_spawning() {
    // A git worker whose worktree claude has not created yet: the row
    // alone says this one cannot start.
    let (ws, key, _db, _project) = stranded_owner_fixture("reviewer");
    ws.insert_live_worker(&key, live_worker_entry("proj", "reviewer"));

    ws.enable_test_dispatch_intercept();
    let outcome = deliver_cron(&ws, "proj", Some("reviewer"), "nightly", false);
    assert!(
        matches!(outcome, crate::spawn::CronFireOutcome::Delivered),
        "a worker still spawning is not unwakeable - its worktree is being created \
             along with it; got {}",
        outcome_name(&outcome),
    );
    assert_eq!(
        parked_crons(&ws, "proj", Some("reviewer")),
        vec!["nightly".to_owned()],
        "the prompt parks for the spawning worker's Connected drain",
    );
}

/// A `Failed` entry is not a live one. `transition_worker_to_failed`
/// keeps it visible on purpose, but the boot wave never starts it, so
/// treating it as wakeable parks the fire in a bucket nothing drains -
/// the unbounded park this whole arm exists to keep empty.
#[test]
fn deliver_worker_cron_does_not_park_for_a_failed_entry() {
    let (ws, key, _db, _project) = stranded_owner_fixture("reviewer");
    let mut failed = live_worker_entry("proj", "reviewer");
    failed.status = forge_primitives::WorkerLiveness::Failed;
    ws.insert_live_worker(&key, failed);

    ws.enable_test_dispatch_intercept();
    let outcome = deliver_cron(&ws, "proj", Some("reviewer"), "nightly", false);
    assert!(
        matches!(outcome, crate::spawn::CronFireOutcome::TargetCannotBeWoken { .. }),
        "a Failed entry is not a worker the wave will start, so its fire must not \
             park; got {}",
        outcome_name(&outcome),
    );
    assert_eq!(
        parked_crons(&ws, "proj", Some("reviewer")).len(),
        0,
        "nothing parks for a worker that will never connect",
    );
}

/// A `Failed` entry's slot can still read as connected when the fire comes;
/// it is not live, so the fire is refused, not dispatched.
#[test]
fn deliver_worker_cron_does_not_dispatch_to_a_connected_failed_entry() {
    let (ws, key, _db, _project) = stranded_owner_fixture("reviewer");
    let mut failed = live_worker_entry("proj", "reviewer");
    failed.status = forge_primitives::WorkerLiveness::Failed;
    ws.insert_live_worker(&key, failed.clone());
    ws.mark_session_connected_for_test(&failed.slot, "reviewer-uuid");

    ws.enable_test_dispatch_intercept();
    let outcome = deliver_cron(&ws, "proj", Some("reviewer"), "nightly", false);
    assert!(
        matches!(outcome, crate::spawn::CronFireOutcome::TargetCannotBeWoken { .. }),
        "a failed worker's slot is not a dispatch target, however connected it still \
             reads; got {}",
        outcome_name(&outcome),
    );
    let dispatched = ws.drain_test_dispatch_buffer();
    assert!(
        !dispatched.iter().any(|c| matches!(
            c,
            Command::Prompt { key, .. } | Command::PromptUnder { key, .. }
                if key == &failed.slot
        )),
        "and no prompt was dispatched into the failed worker: {dispatched:?}",
    );
}

#[tokio::test]
async fn deliver_asleep_dynamic_worker_cron_buffers_and_wakes_the_project() {
    let (ws, _rx) = Workspace::testing_stub();
    let dir = tempdir().expect("tempdir");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    let key = seed_project_with_a_real_root(&ws, &dir);
    // "scratch" exists only via its persisted worker row.
    seed_worker_row(&ws, &key, "scratch");

    ws.enable_test_dispatch_intercept();
    let outcome = deliver_cron(&ws, "proj", Some("scratch"), "hourly", false);
    assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
    let dispatched = ws.drain_test_dispatch_buffer();
    assert!(
        dispatched.iter().any(|c| matches!(
            c, Command::SpawnProject { project_name, .. } if project_name == "proj"
        )),
        "an asleep dynamic worker cron wakes the project too",
    );
    assert_eq!(
        parked_crons(&ws, "proj", Some("scratch")).len(),
        1,
        "buffered for the dynamic worker owner"
    );
}

/// A standing cron that may not wake its owner (#2423/C): while the
/// owner sleeps the fire is skipped and the entry stays due, so the
/// next session start (or a tick while it is awake) runs it. The same
/// entry with `wake: true` spawns, which is the control.
#[test]
fn a_no_wake_cron_skips_a_sleeping_owner_and_stays_due() {
    let (ws, _rx) = Workspace::testing_stub();
    ws.seed_test_project("proj", "/tmp/wc-nowake");
    ws.enable_test_dispatch_intercept();
    let now = std::time::SystemTime::now();

    let mut sweep = schedule("sweep", "proj", "sweep", None);
    sweep.next_fire = std::time::SystemTime::UNIX_EPOCH;
    sweep.wake = false;
    ws.push_cron(sweep);
    ws.fire_due_crons(now);

    let stored = ws.crons_for_project("proj");
    assert_eq!(stored.len(), 1, "the skipped fire left the cron in place");
    assert_eq!(stored[0].last_fire, None, "and did not count as a fire");
    assert_eq!(
        stored[0].next_fire,
        std::time::SystemTime::UNIX_EPOCH,
        "the entry stays due, so a session start runs it",
    );
    assert!(ws.drain_test_dispatch_buffer().is_empty(), "no session was woken");
    assert!(parked_crons(&ws, "proj", None).is_empty(), "and nothing was parked");

    // Control: the same entry with wake on spawns its sleeping owner.
    let mut wakeful = schedule("wakeful", "proj", "stand-up", None);
    wakeful.next_fire = std::time::SystemTime::UNIX_EPOCH;
    ws.push_cron(wakeful);
    ws.fire_due_crons(now);
    assert!(!ws.drain_test_dispatch_buffer().is_empty(), "a wakeful cron still wakes its owner");
    assert_eq!(
        ws.crons_for_project("proj")
            .iter()
            .find(|cron| cron.id == forge_primitives::cron::CronId::from("wakeful"))
            .and_then(|cron| cron.last_fire),
        Some(now),
        "and its fire advances the schedule",
    );
}

/// A no-wake cron is skipped only while the owner SLEEPS: with a live
/// lead the fire lands and the schedule advances, which is the
/// standing cadence (#2423/C).
#[test]
fn a_no_wake_cron_fires_normally_while_the_owner_is_awake() {
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/wc-nowake-awake");
    let cwd = project_expanded_path(&ws, "proj");
    ws.record_connected_session(&cwd, "lead-uuid", None);
    let lead = SessionSlot::lead("TestOrg", "proj");
    let (handle, _agent_rx) = Workspace::testing_stub_handle();
    ws.pool.lock().insert(
        lead.clone(),
        PooledAgent {
            handle: Arc::new(handle),
            account: AccountKey("test".to_owned()),
            permission_mode: None,
            registration: None,
            session_id: "pooled-session".to_owned(),
        },
    );
    ws.mark_session_connected_for_test(&lead, "lead-uuid");

    let now = std::time::SystemTime::now();
    let mut sweep = schedule("sweep", "proj", "sweep", None);
    sweep.next_fire = std::time::SystemTime::UNIX_EPOCH;
    sweep.wake = false;
    ws.push_cron(sweep);
    ws.enable_test_dispatch_intercept();

    ws.fire_due_crons(now);
    let texts: Vec<String> = ws
        .drain_test_dispatch_buffer()
        .iter()
        .filter_map(|command| match command {
            Command::Prompt { text, .. } | Command::PromptUnder { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("sweep")),
        "the awake owner is given its sweep: {texts:?}",
    );
    assert_eq!(
        ws.crons_for_project("proj")
            .iter()
            .find(|cron| cron.id == forge_primitives::cron::CronId::from("sweep"))
            .and_then(|cron| cron.last_fire),
        Some(now),
        "and the fire advances the schedule",
    );
}

#[test]
fn deliver_worker_cron_with_owner_conclusively_gone_is_target_gone() {
    let (ws, _rx) = Workspace::testing_stub();
    let dir = tempdir().expect("tempdir");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    // Db open, empty session store: the row is the only thing that
    // could bring this owner back, so "ghost" is conclusively absent
    // and its cron must be collected rather than buffered into a
    // bucket nothing will ever drain.
    ws.seed_test_project("proj", "/tmp/wc-gone");
    let outcome = deliver_cron(&ws, "proj", Some("ghost"), "x", false);
    assert!(
        matches!(outcome, crate::spawn::CronFireOutcome::TargetGone),
        "a label with no row in the session store is conclusively gone",
    );
}

/// A cron's owner can have a row and still be unwakeable: the row
/// says it runs in a worktree, and the worktree is gone, so the wave
/// skips it and nothing is left to drain a parked prompt. Buffering
/// would grow the bucket once per fire until process exit, and
/// reporting Delivered would advance the watermark past a prompt that
/// never reached anyone.
///
/// The control half is the same fire once the worktree stands: it is
/// delivered and parked, so the refusal above is the missing
/// directory and not the fixture.
#[test]
fn deliver_worker_cron_with_an_unwakeable_owner_is_neither_parked_nor_delivered() {
    let (ws, _rx) = Workspace::testing_stub();
    let dir = tempdir().expect("tempdir");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    let project_dir = tempdir().expect("project dir");
    ws.seed_test_project("proj", &project_dir.path().to_string_lossy());
    let key = ws.project_key_for_name("proj").expect("seeded project");
    ws.record_worker_row(
        &key,
        "steward",
        "steward-uuid",
        "c",
        None,
        None,
        false,
        crate::mcp::workers::types::WorkerTree { is_git_repo: true, cwd: None },
        None,
    )
    .expect("seed the worker's row");

    let outcome = deliver_cron(&ws, "proj", Some("steward"), "x", false);
    let crate::spawn::CronFireOutcome::TargetCannotBeWoken { directory } = outcome else {
        panic!("an owner whose worktree is gone cannot be woken, so its fire is not delivered");
    };
    assert_eq!(
        directory,
        project_dir.path().join(".claude").join("worktrees").join("steward"),
        "the outcome has to carry the directory the fire was waiting for: it is the one \
             thing an operator can restore",
    );
    assert_eq!(
        parked_crons(&ws, "proj", Some("steward")).len(),
        0,
        "nothing parks for an owner that can never drain the bucket",
    );

    std::fs::create_dir_all(project_dir.path().join(".claude").join("worktrees").join("steward"))
        .expect("restore the worktree");
    let delivered = deliver_cron(&ws, "proj", Some("steward"), "x", false);
    assert!(
        matches!(delivered, crate::spawn::CronFireOutcome::Delivered),
        "the same fire is delivered once the worktree is back, so the refusal above \
             was the missing directory and not the fixture",
    );
    assert_eq!(
        parked_crons(&ws, "proj", Some("steward")).len(),
        1,
        "and its prompt is parked for the owner to drain on connect",
    );
}

/// The directory this fire waits on is the ROW's tree, not the
/// project's: a worker the spawn pointed at another repository starts
/// in THAT repository's worktree, so an operator has to be told to
/// restore that one. The project's own worktree stands here, so a
/// wake composing the project path would call the owner wakeable and
/// park a prompt nothing would drain.
#[test]
fn a_cron_wake_reads_the_rows_own_tree() {
    let (ws, _rx) = Workspace::testing_stub();
    let dir = tempdir().expect("tempdir");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    let project_dir = tempdir().expect("project dir");
    ws.seed_test_project("proj", &project_dir.path().to_string_lossy());
    let key = ws.project_key_for_name("proj").expect("seeded project");
    // The project's own worktree stands: anything composing the
    // project path answers "the owner can start".
    std::fs::create_dir_all(project_dir.path().join(".claude").join("worktrees").join("steward"))
        .expect("create the project worktree");
    // The row's own tree, with no worktree under it.
    let worker_tree = tempdir().expect("worker tree");
    let root = std::fs::canonicalize(worker_tree.path()).expect("canonical tree");
    ws.seed_test_worker_row_at(&key, "steward", &root.to_string_lossy(), true);

    let outcome = deliver_cron(&ws, "proj", Some("steward"), "x", false);
    let crate::spawn::CronFireOutcome::TargetCannotBeWoken { directory } = outcome else {
        panic!("an owner whose own tree has no worktree cannot be woken");
    };
    assert_eq!(
        directory,
        root.join(".claude").join("worktrees").join("steward"),
        "the outcome names the tree an operator has to restore - the worker's own",
    );
}

#[test]
fn deliver_worker_cron_leaves_it_when_the_owner_check_cannot_read() {
    // No db installed, so the durable-worker lookup fails: absence can't
    // be confirmed, so the cron must be left (retried next tick), not
    // deleted as owner-gone.
    let (ws, _rx) = Workspace::testing_stub();
    ws.seed_test_project("proj", "/tmp/wc-unknown");
    let outcome = deliver_cron(&ws, "proj", Some("scratch"), "x", false);
    assert!(
        matches!(outcome, crate::spawn::CronFireOutcome::DispatchFailed),
        "a failed owner check leaves the cron for the next tick, not TargetGone",
    );
}

/// A spawn needs the account map settled, and one half of that is the
/// gateway listener, which the boot catch-up can outrun. That refusal
/// is transient, so the fire stays unconsumed for the next tick
/// rather than advancing past a prompt that never landed.
#[test]
fn deliver_cron_with_the_listener_unbound_leaves_the_fire_for_the_next_tick() {
    let (ws, _rx) = Workspace::testing_stub();
    ws.seed_test_project("proj", "/tmp/wc-gate");
    ws.enable_test_dispatch_intercept();

    // Control: with the listener bound this same fire is delivered, so
    // the refusal below is the map and not the owner check.
    let bound = deliver_cron(&ws, "proj", None, "x", false);
    assert!(
        matches!(bound, crate::spawn::CronFireOutcome::Delivered),
        "the same fire with the listener bound is delivered",
    );
    ws.drain_test_dispatch_buffer();

    ws.seed_test_gateway_ready(false);
    let unbound = deliver_cron(&ws, "proj", None, "x", false);
    assert!(
        matches!(unbound, crate::spawn::CronFireOutcome::DispatchFailed),
        "an unbound listener is a transient refusal, not a delivered fire",
    );
    assert_eq!(
        parked_crons(&ws, "proj", None).len(),
        1,
        "the deferred fire parked nothing, so the retry parks it once rather than twice",
    );
}

/// The other half of an unsettled map is the accounts themselves: the
/// walk skips every account still `Loading`, so a project whose only
/// account has not settled is refused there. Also pins the boundary -
/// `Bailed` is terminal, so it settles the map and the walk falls back
/// to it rather than refusing.
#[test]
fn deliver_cron_with_an_unsettled_account_map_leaves_the_fire_for_the_next_tick() {
    let (ws, _dir) = workspace_with_one_unsettled_account();
    ws.enable_test_dispatch_intercept();

    // Nothing runs the account loader here, so the one account starts
    // `Loading`: the fire is left rather than parked.
    let unsettled = deliver_cron(&ws, "proj", None, "x", false);
    assert_eq!(
        outcome_name(&unsettled),
        "DispatchFailed",
        "an account still loading is a transient refusal, not a delivered fire",
    );
    assert!(
        parked_crons(&ws, "proj", None).is_empty(),
        "and nothing is parked, so the retry parks it once rather than twice",
    );

    // Controls: the same fire is delivered once the map settles, so
    // the refusal above is the account map and not the owner check.
    ws.seed_test_ready_account("acct-a");
    let ready = deliver_cron(&ws, "proj", None, "x", false);
    assert_eq!(
        outcome_name(&ready),
        "Delivered",
        "the same fire with the account settled is delivered",
    );

    // `Bailed` is terminal too, so it settles the map as well, and the
    // walk falls back to it as the last resort rather than refusing.
    ws.seed_test_account_state("acct-a", forge_gateway::LoadingState::Bailed);
    let bailed = deliver_cron(&ws, "proj", None, "x", false);
    assert_eq!(
        outcome_name(&bailed),
        "Delivered",
        "a Bailed account settles the map and the walk falls back to it",
    );
    assert_eq!(parked_crons(&ws, "proj", None).len(), 2, "both settled fires parked their prompt");
}

/// A cooldown empties the project's walk until its reset, which is
/// transient the same way an unsettled map is, so the fire stays due
/// rather than parking a prompt the refusal would expire.
#[test]
fn deliver_cron_during_a_cooldown_leaves_the_fire_for_the_next_tick() {
    let (ws, _dir) = workspace_with_one_unsettled_account();
    ws.seed_test_ready_account("acct-a");
    ws.enable_test_dispatch_intercept();

    // Control: with the one account serving and not cooling, the same
    // fire is delivered.
    let served = deliver_cron(&ws, "proj", None, "x", false);
    assert_eq!(
        outcome_name(&served),
        "Delivered",
        "the same fire with the account serving is delivered",
    );

    // The usage probe's own verdict is the public way to cool an
    // account, and it takes epoch seconds.
    let reset_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_secs()
        + 60;
    ws.gateway.report_probe_limit(&forge_gateway::AccountKey("acct-a".to_owned()), Some(reset_at));
    let cooling = deliver_cron(&ws, "proj", None, "x", false);
    assert_eq!(
        outcome_name(&cooling),
        "DeferredCooling",
        "a cooling walk is a transient refusal, not a delivered fire",
    );
    assert_eq!(
        parked_crons(&ws, "proj", None).len(),
        1,
        "the deferred fire parked nothing, so the retry parks it once rather than twice",
    );
}

/// The outcome's own name, so a failure says which variant it got
/// rather than only that it was not the expected one.
fn outcome_name(outcome: &crate::spawn::CronFireOutcome) -> &'static str {
    match outcome {
        crate::spawn::CronFireOutcome::Delivered => "Delivered",
        crate::spawn::CronFireOutcome::TargetGone => "TargetGone",
        crate::spawn::CronFireOutcome::TargetCannotBeWoken { .. } => "TargetCannotBeWoken",
        crate::spawn::CronFireOutcome::DeferredCooling { .. } => "DeferredCooling",
        crate::spawn::CronFireOutcome::DispatchFailed => "DispatchFailed",
        crate::spawn::CronFireOutcome::SkippedAsleep => "SkippedAsleep",
    }
}

/// A workspace over a forge.toml declaring one account and one project
/// with the model that account serves, so the account walk is the
/// thing a spawn would reach. Nothing runs the account loader in a
/// test, so the account map starts unsettled. The tempdir must
/// outlive the caller.
///
/// `new_for_test` rather than a stubbed workspace on purpose: the
/// stubs carry an EMPTY account pool, where `all_loaded` is vacuously
/// true and `set_loading` is a no-op, so they cannot tell the two
/// halves of the predicate apart.
fn workspace_with_one_unsettled_account() -> (std::sync::Arc<crate::Workspace>, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
    std::fs::write(
        forge_dir.join("forge.toml"),
        r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]

[[orgs.projects]]
name = "proj"
path = "/tmp/wc-unsettled"
model = "claude-sonnet-5"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
    )
    .expect("write forge.toml");
    let ws = std::sync::Arc::new(
        crate::Workspace::new_for_test(dir.path().to_owned()).expect("boot from the fixture"),
    );
    (ws, dir)
}

#[test]
fn deliver_cron_marks_an_overdue_fire_as_missed() {
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/wc-missed");
    let cwd = project_expanded_path(&ws, "proj");
    ws.record_connected_session(&cwd, "lead-uuid", None);
    let lead_key = SessionSlot::lead("TestOrg", "proj");
    let (handle, _agent_rx) = Workspace::testing_stub_handle();
    ws.pool.lock().insert(
        lead_key.clone(),
        PooledAgent {
            handle: Arc::new(handle),
            account: AccountKey("test".to_owned()),
            permission_mode: None,
            registration: None,
            session_id: "pooled-session".to_owned(),
        },
    );

    ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
    ws.enable_test_dispatch_intercept();
    deliver_cron(&ws, "proj", None, "standup", true);
    deliver_cron(&ws, "proj", None, "standup", false);
    let dispatched = ws.drain_test_dispatch_buffer();
    let texts: Vec<String> = dispatched
        .iter()
        .filter_map(|c| match c {
            Command::Prompt { text, .. } | Command::PromptUnder { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.contains(&"[missed cron] standup".to_owned()),
        "an overdue fire is delivered with the missed marker",
    );
    assert!(texts.contains(&"standup".to_owned()), "an on-time fire has no marker");
}

#[test]
fn fire_due_crons_missed_threshold_is_two_ticks() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/wc-thresh");
    let cwd = project_expanded_path(&ws, "proj");
    ws.record_connected_session(&cwd, "lead-uuid", None);
    let lead_key = SessionSlot::lead("TestOrg", "proj");
    let (handle, _agent_rx) = Workspace::testing_stub_handle();
    ws.pool.lock().insert(
        lead_key.clone(),
        PooledAgent {
            handle: Arc::new(handle),
            account: AccountKey("test".to_owned()),
            permission_mode: None,
            registration: None,
            session_id: "pooled-session".to_owned(),
        },
    );
    ws.mark_session_connected_for_test(&lead_key, "lead-uuid");

    let now = std::time::SystemTime::now();
    let cron = |id: &str, prompt: &str, next_fire| CronEntry {
        id: CronId::from(id),
        project_name: "proj".to_owned(),
        kind: CronKind::Recurring("*/5 * * * *".to_owned()),
        prompt: prompt.to_owned(),
        created_at: std::time::SystemTime::UNIX_EPOCH,
        description: None,
        last_fire: None,
        next_fire,
        team_role: None,
        wake: true,
    };
    // Due one tick ago: within the jitter window, on-time. Due three
    // ticks ago: a genuine catch-up, missed.
    ws.push_cron(cron("recent", "recent", now - std::time::Duration::from_secs(60)));
    ws.push_cron(cron("stale", "stale", now - std::time::Duration::from_secs(180)));

    ws.enable_test_dispatch_intercept();
    ws.fire_due_crons(now);
    let texts: Vec<String> = ws
        .drain_test_dispatch_buffer()
        .iter()
        .filter_map(|c| match c {
            Command::Prompt { text, .. } | Command::PromptUnder { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&"recent".to_owned()), "a one-tick-late fire is on-time");
    assert!(
        texts.contains(&"[missed cron] stale".to_owned()),
        "a three-tick-late fire is marked missed",
    );
}

#[test]
fn fire_due_crons_removes_a_cron_whose_project_is_gone() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    // No db installed, so push_cron stays in memory - no config dir needed.
    let (ws, _rx) = Workspace::testing_stub();
    let now = std::time::SystemTime::now();

    ws.push_cron(CronEntry {
        id: CronId::from("orphan"),
        project_name: "deleted-project".to_owned(),
        kind: CronKind::Recurring("*/5 * * * *".to_owned()),
        prompt: "p".to_owned(),
        created_at: std::time::SystemTime::UNIX_EPOCH,
        description: None,
        last_fire: None,
        next_fire: std::time::SystemTime::UNIX_EPOCH, // overdue -> due now
        team_role: None,
        wake: true,
    });

    ws.enable_test_dispatch_intercept();
    ws.fire_due_crons(now);
    let dispatched = ws.drain_test_dispatch_buffer();

    assert!(
        dispatched.iter().all(|c| !matches!(c, crate::protocol::Command::SpawnProject { .. })),
        "a cron whose project is gone gets no spawn",
    );
    assert!(
        ws.crons_for_project("deleted-project").is_empty(),
        "an overdue cron whose project left forge.toml is removed, not advanced forever",
    );
}

/// The router's fate for a fire whose owner cannot be woken. A
/// recurring cron has another slot coming, so this fire is dropped and
/// the schedule advances. A one-shot has no later slot and advancing
/// removes it, so it stays due: dropping it would throw away the only
/// prompt that cron will ever carry.
///
/// Neither may park - that bucket has nothing to drain it.
#[test]
fn fire_due_crons_advances_recurring_and_keeps_a_one_shot_when_the_owner_cannot_be_woken() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let (ws, _key, _db, _project) = stranded_owner_fixture("steward");

    let now = std::time::SystemTime::now();
    let past = std::time::SystemTime::UNIX_EPOCH;
    for (id, kind) in [
        ("recurring", CronKind::Recurring("*/5 * * * *".to_owned())),
        ("one-shot", CronKind::Once(past)),
    ] {
        ws.push_cron(CronEntry {
            id: CronId::from(id),
            project_name: "proj".to_owned(),
            kind,
            prompt: "p".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: past,
            team_role: Some("steward".to_owned()),
            wake: true,
        });
    }

    ws.enable_test_dispatch_intercept();
    ws.fire_due_crons(now);

    let after = ws.crons_for_project("proj");
    let ids: Vec<String> = after.iter().map(|c| c.id.0.clone()).collect();
    let recurring = after.iter().find(|c| c.id == CronId::from("recurring"));
    assert!(
        recurring.is_some_and(|c| c.last_fire.is_some()),
        "a recurring cron's fire is dropped and its schedule advances, rather than \
             the entry being removed; entries left {ids:?}",
    );
    assert!(
        after.iter().any(|c| c.id == CronId::from("one-shot")),
        "a one-shot has no later slot, so it stays due rather than being removed \
             along with its prompt; entries left {ids:?}",
    );
    assert_eq!(
        parked_crons(&ws, "proj", Some("steward")).len(),
        0,
        "nothing parks for an owner that can never drain the bucket",
    );
}

#[test]
fn advance_or_remove_cron_removes_never_occurring_recurring() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    // "0 0 30 2 *" (Feb 30) has no upcoming occurrence, so
    // next_fire_after returns None and the entry is removed (with a
    // warn) rather than left stuck at a stale next_fire.
    ws.push_cron(CronEntry {
        id: CronId::from("impossible"),
        project_name: "forge".to_owned(),
        kind: CronKind::Recurring("0 0 30 2 *".to_owned()),
        prompt: "p".to_owned(),
        created_at: std::time::SystemTime::UNIX_EPOCH,
        description: None,
        last_fire: None,
        next_fire: std::time::SystemTime::UNIX_EPOCH,
        team_role: None,
        wake: true,
    });
    ws.advance_or_remove_cron(&CronId::from("impossible"), std::time::SystemTime::now());
    assert!(
        ws.crons_for_project("forge").is_empty(),
        "a recurring cron with no upcoming occurrence is removed, not left stuck",
    );
}

/// The registry's idempotency: the first `start_cron_scheduler`
/// registers the member and runs exactly one tick loop, and a second
/// call is a no-op. The due cron is the observable that the start
/// opens - a start that never runs fires nothing.
#[tokio::test(start_paused = true)]
async fn start_cron_scheduler_fires_a_due_cron_once_despite_a_second_start() {
    use forge_primitives::cron::{CronEntry, CronId, CronKind};
    let dir = tempdir().expect("tempdir");
    let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
    ws.seed_test_project("proj", "/tmp/wc-scheduler-once");

    ws.push_cron(CronEntry {
        id: CronId::from("due"),
        project_name: "proj".to_owned(),
        kind: CronKind::Recurring("*/5 * * * *".to_owned()),
        prompt: "tick".to_owned(),
        created_at: std::time::SystemTime::UNIX_EPOCH,
        description: None,
        last_fire: None,
        next_fire: std::time::SystemTime::UNIX_EPOCH, // overdue -> due at the first tick
        team_role: None,
        wake: true,
    });

    ws.enable_test_dispatch_intercept();
    ws.start_cron_scheduler();
    assert!(
        ws.supervisor().snapshot().iter().any(|member| member.name == "cron_scheduler"),
        "the first start registers the scheduler",
    );

    // Let the spawned task poll once so its interval is registered
    // at t=0 (first tick at +60s) before the clock moves.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::time::advance(CRON_TICK_INTERVAL).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    let first_tick = count_proj_spawns(&ws);
    assert_eq!(
        first_tick, 1,
        "the first start runs the scheduler; a guard that never opens fires nothing",
    );

    // A second start must not spawn a second tick loop. Fires
    // cannot tell two loops apart - ticks run sequentially here and
    // the first loop's advance hides every due cron from the second
    // (on the multi-thread runtime their ticks race, which is what
    // the guard prevents) - so its only trace is another task-held
    // Weak.
    let weak_before = Arc::downgrade(&ws).weak_count();
    ws.start_cron_scheduler();
    let weak_after = Arc::downgrade(&ws).weak_count();
    assert_eq!(weak_after, weak_before, "a second start must not spawn a second tick loop");
}

fn count_proj_spawns(ws: &Workspace) -> usize {
    ws.drain_test_dispatch_buffer()
        .iter()
        .filter(|c| {
            matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "proj")
        })
        .count()
}

/// The tick loop holds the workspace only weakly: once the caller's
/// `Arc` is gone the workspace drops even though the task is parked
/// on its next tick. Advancing past a tick drives the
/// upgrade-failure exit; a task that moved a strong `Arc` in would
/// keep the workspace alive and fail the upgrade assert.
#[tokio::test(start_paused = true)]
async fn the_scheduler_task_does_not_hold_the_workspace_alive() {
    let dir = tempdir().expect("tempdir");
    let weak = {
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.start_cron_scheduler();
        // Poll the task once so its interval is registered and it
        // sits parked on the next tick, holding only the Weak.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        Arc::downgrade(&ws)
    };
    tokio::time::advance(CRON_TICK_INTERVAL * 2).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(weak.upgrade().is_none(), "the scheduler task must hold only a Weak");
}

/// The cron tools' caller-scoped view over a real workspace: the
/// production facade resolves the caller through `forge.toml` and the
/// worker registry, which is what these tests exercise.
mod prod_facade_tests {
    use super::*;
    use std::time::SystemTime;

    use forge_primitives::WorkerLiveness;
    use forge_primitives::cron::{CronId, CronKind};
    use forge_tools::cron::facade::{
        CronDeleteError, CronFacade, CronFacadeHost, CronListError, ProdCronFacade,
    };

    use crate::WorkerEntry;

    /// The production facade over one workspace: the subsystem handle and
    /// the host wiring, both weak, as the spawn path builds it.
    fn prod_cron_facade(ws: &Arc<Workspace>) -> Arc<dyn CronFacade> {
        let host: Arc<dyn CronFacadeHost> = ws.clone();
        Arc::new(ProdCronFacade::new(Arc::downgrade(&ws.cron), Arc::downgrade(&host)))
    }

    fn worker_entry(project: &str, label: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "review".to_owned(),
            slot: SessionSlot::worker("TestOrg", project, label),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", project),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            cwd: None,
            diagnostic: None,
            kick: None,
        }
    }

    fn fixture() -> (Arc<Workspace>, Arc<dyn CronFacade>, SessionSlot, SessionSlot) {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/b2-myproj");
        let key =
            ws.list_projects().into_iter().find(|v| v.name == "myproj").expect("seeded view").key;
        ws.record_connected_session("/tmp/b2-myproj", "lead-uuid", None);
        ws.insert_live_worker(&key, worker_entry("myproj", "reviewer"));
        let facade = prod_cron_facade(&ws);
        (
            ws,
            facade,
            SessionSlot::lead("TestOrg", "myproj"),
            SessionSlot::worker("TestOrg", "myproj", "reviewer"),
        )
    }

    fn daily(prompt: &str) -> (CronKind, String) {
        (CronKind::Recurring("0 9 * * *".to_owned()), prompt.to_owned())
    }

    #[test]
    fn create_stamps_owner_and_list_delete_are_owner_scoped() {
        let (_ws, facade, lead, worker) = fixture();

        let (lk, lp) = daily("lead-standup");
        let lead_cron = facade
            .create_cron(&lead, lk, lp, Some("Lead stand-up".to_owned()), true)
            .expect("lead create");
        assert_eq!(lead_cron.team_role, None, "a lead cron is owner-less");
        assert_eq!(
            lead_cron.description.as_deref(),
            Some("Lead stand-up"),
            "the supplied description persists on the entry",
        );
        let (wk, wp) = daily("worker-standup");
        let worker_cron = facade.create_cron(&worker, wk, wp, None, true).expect("worker create");
        assert_eq!(worker_cron.description, None, "no description stays None");
        assert_eq!(
            worker_cron.team_role.as_deref(),
            Some("reviewer"),
            "a worker cron is stamped with its label",
        );

        let worker_list = facade.list_crons(&worker).expect("list");
        assert_eq!(worker_list.len(), 1, "the worker sees only its own cron");
        assert_eq!(worker_list[0].id, worker_cron.id);
        let lead_list = facade.list_crons(&lead).expect("list");
        assert_eq!(lead_list.len(), 1, "the lead sees only lead crons");
        assert_eq!(lead_list[0].id, lead_cron.id);

        assert_eq!(
            facade.delete_cron(&worker, &lead_cron.id),
            Err(CronDeleteError::NotOwnedByCaller),
            "a worker cannot delete a lead's cron, and the refusal says which case it is",
        );
        assert_eq!(
            facade.delete_cron(&lead, &worker_cron.id),
            Err(CronDeleteError::NotOwnedByCaller),
            "a lead cannot delete a worker's cron",
        );
        facade.delete_cron(&worker, &worker_cron.id).expect("a worker deletes its own cron");
    }

    /// The delete hands back the entry it removed, so the tool can echo
    /// the schedule and prompt that went rather than a bare id.
    ///
    /// **A neighbour is registered FIRST**, so the removed set has a
    /// record before the named one: an implementation answering with "the
    /// first entry" instead of "the entry the id named" echoes the
    /// neighbour here and leaves the named entry in the store, and both
    /// assertions catch it.
    #[test]
    fn delete_returns_the_entry_it_removed() {
        let (_ws, facade, lead, _worker) = fixture();
        let (kind, prompt) = daily("worker digest");
        let neighbour = facade
            .create_cron(&lead, kind, prompt, Some("Worker digest".to_owned()), true)
            .expect("create the neighbour first");
        let (kind, prompt) = daily("morning summary");
        let cron = facade
            .create_cron(&lead, kind, prompt, Some("Morning summary".to_owned()), true)
            .expect("create");

        let removed = facade.delete_cron(&lead, &cron.id).expect("the lead deletes its own cron");

        assert_eq!(removed.id, cron.id, "the echoed entry is the cron that went");
        assert_eq!(removed.prompt, "morning summary");
        assert_eq!(removed.description.as_deref(), Some("Morning summary"));
        assert!(
            matches!(&removed.kind, CronKind::Recurring(expr) if expr == "0 9 * * *"),
            "carrying the schedule it was registered under: {:?}",
            removed.kind,
        );
        let left = facade.list_crons(&lead).expect("list");
        assert_eq!(left.len(), 1, "only the neighbour survives");
        assert_eq!(left[0].id, neighbour.id, "and it is the neighbour that survives");
    }

    const ONE_PROJECT_CONFIG: &str = r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]

[[orgs.projects]]
name = "forge"
path = "/tmp/cron-store-fixture"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#;

    fn booted_workspace(dir: &tempfile::TempDir) -> Arc<Workspace> {
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), ONE_PROJECT_CONFIG).expect("write forge.toml");
        Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace boots"))
    }

    /// A boot whose store could not open leaves the in-memory set empty
    /// while the rows may still be on disk. An empty answer then reads as
    /// "you have no crons", and a cleanup that trusts it leaves the old
    /// ones firing.
    #[test]
    fn list_refuses_when_the_boot_could_not_load_the_store() {
        // A file where the app-support base must be a directory, so the
        // stores dir cannot be created and the boot opens no DB.
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), ONE_PROJECT_CONFIG).expect("write forge.toml");
        std::fs::write(dir.path().join("app-support"), b"not a directory")
            .expect("block the app-support base");

        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace boots"));
        let facade = prod_cron_facade(&ws);

        assert_eq!(
            facade.list_crons(&SessionSlot::lead("TestOrg", "forge")),
            Err(CronListError::Unavailable),
            "a boot that could not load its durable crons must not answer as an empty registry",
        );
    }

    /// The boot's other failing arm: the store opens, but its cron read
    /// fails. A regression dropping the read arm's `true` would restore
    /// the empty-list lie while the store-open test above stays green.
    #[test]
    fn list_refuses_when_the_open_store_could_not_read_its_crons() {
        use redb::TableDefinition;

        // The right table name under the wrong key/value types: the store
        // opens, but opening it as the cron table fails.
        const WRONG_CRONS: TableDefinition<u64, u64> = TableDefinition::new("crons");

        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), ONE_PROJECT_CONFIG).expect("write forge.toml");
        let app_support = dir.path().join("app-support");
        std::fs::create_dir_all(app_support.join("stores")).expect("stores/");
        let canonical = dir.path().canonicalize().expect("canonical config dir");
        let db_path = crate::store::path_for(&app_support, &canonical);
        {
            let db = crate::store::Db::open(&db_path).expect("open db");
            let txn = db.database().begin_write().expect("write txn");
            {
                txn.open_table(WRONG_CRONS).expect("create the mis-typed crons table");
            }
            txn.commit().expect("commit");
        }

        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace boots"));
        let facade = prod_cron_facade(&ws);

        assert!(ws.db.lock().is_some(), "the store opened; only its cron read fails");
        assert!(
            ws.durable_crons_unavailable(),
            "a failed cron read marks the registry unavailable",
        );
        assert_eq!(
            facade.list_crons(&SessionSlot::lead("TestOrg", "forge")),
            Err(CronListError::Unavailable),
            "a boot whose store opened but whose cron read failed must not answer as empty",
        );
    }

    /// The negative control for the refusals above: a healthy boot opens
    /// the store, so an empty registry is a real answer rather than a
    /// failed load.
    #[test]
    fn a_healthy_boot_reads_an_empty_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = booted_workspace(&dir);
        let facade = prod_cron_facade(&ws);

        assert!(
            !ws.durable_crons_unavailable(),
            "a boot whose store opened is not an unavailable run",
        );
        assert_eq!(
            facade.list_crons(&SessionSlot::lead("TestOrg", "forge")),
            Ok(Vec::new()),
            "a healthy boot with no crons answers empty rather than refusing",
        );
    }

    /// The two halves the lead's answer actually composes: the production
    /// facade over a boot with no durable store, driven through the tool.
    #[tokio::test]
    async fn the_tool_refuses_over_the_prod_facade_when_the_store_could_not_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), ONE_PROJECT_CONFIG).expect("write forge.toml");
        std::fs::write(dir.path().join("app-support"), b"not a directory")
            .expect("block the app-support base");

        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace boots"));
        let facade = prod_cron_facade(&ws);
        let tools =
            forge_tools::cron::CronTools::new(facade, SessionSlot::lead("TestOrg", "forge"));

        let out = forge_mcp::ToolProvider::call(&tools, "cron__list", serde_json::json!({}))
            .await
            .expect("the cron family owns cron__list")
            .expect("the tool answers");
        let text = crate::mcp::test_support::text_of(&out);
        assert!(out.is_error, "the composed answer is an error: {text}");
        assert!(
            text.contains("couldn't load durable crons"),
            "the composed answer names the failure: {text}",
        );
    }

    /// **An unresolvable caller is a refusal from the real facade**, not
    /// an empty registry: the production arm is what the tool's error
    /// renders.
    #[test]
    fn list_refuses_a_caller_that_maps_to_no_project() {
        let (_ws, facade, _lead, _worker) = fixture();

        assert_eq!(
            facade.list_crons(&SessionSlot::lead("TestOrg", "no-such-project")),
            Err(CronListError::UnknownCallerProject),
            "a caller outside every project must not read as a caller with no crons",
        );
    }

    /// The other half of the refusal: an id the project never had is a
    /// different answer from one that exists under another session.
    #[test]
    fn delete_refuses_an_id_the_project_never_had() {
        let (_ws, facade, lead, _worker) = fixture();

        assert_eq!(
            facade.delete_cron(&lead, &CronId::from("ghost")),
            Err(CronDeleteError::NoSuchCron),
            "an id with no entry in the project is reported as absent",
        );
    }

    /// Both clauses of that message are about the caller's own project:
    /// an id living in another project is not "in this project but not
    /// registered by you", it is not in this project at all.
    #[test]
    fn delete_refuses_an_id_from_another_project_as_absent() {
        let (ws, facade, lead, _worker) = fixture();
        ws.seed_test_project("otherproj", "/tmp/b2-otherproj");
        let other = SessionSlot::lead("TestOrg", "otherproj");
        let (kind, prompt) = daily("other-standup");
        let other_cron = facade
            .create_cron(&other, kind, prompt, None, true)
            .expect("the other project's create");

        assert_eq!(
            facade.delete_cron(&lead, &other_cron.id),
            Err(CronDeleteError::NoSuchCron),
            "an id that lives in another project is absent here, not a cron of this project's",
        );
    }
}
