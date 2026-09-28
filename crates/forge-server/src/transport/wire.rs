//! A subject's wire form, and the one place it is decided.
//!
//! The server hands a view the surface's own values, and a client reads a
//! shape built for it here rather than the core's structs. A twin per
//! subject keeps a wire change in one file instead of in a handler and a
//! test, and the fixtures beside this module are what make a changed shape
//! fail a test rather than a client.
//!
//! **The twins are complete.** A field is carried unless it is a Rust
//! mechanism rather than data - a `oneshot`, an `Arc<Workspace>` - because
//! the client is not today's pages: it is built against this wire and
//! compared with the terminal, and a field missing here cannot be added
//! later without a server change.

use std::path::Path;

use anyhow::Result;
use forge_primitives::review::{ReviewSet, ReviewThread};
use forge_primitives::runtime::{AvailableAgent, AvailableCommand, MonitorRecord};
use forge_primitives::slack::SlackSubscription;
use forge_primitives::{GotifySubscription, SessionSlot};
use forge_workspace::env::processes::ProcessSnapshot;
use forge_workspace::{AccountLoadingRow, GatewayOrgView, McpServers, ProjectView, WorkerEntry};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::file_index::FileIndex;
use crate::surface::{AgentRow, PendingAsk, ViewSurface};
use crate::transport::TransportState;
use crate::transport::envelope::Subject;
use crate::work::WorkState;

/// The seat the fixture surface is populated for.
///
/// One seat is enough: a fixture pins the SHAPE of a snapshot, and a second
/// seat would pin the same shape twice.
pub fn fixture_seat() -> SessionSlot {
    SessionSlot::lead("TestOrg", "proj")
}

/// One of the surface's two independent review reads: the records, or why
/// they could not be read. They fail apart, so neither is collapsed into
/// the other, and a client can tell an empty branch from an unreadable one.
#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum ReadWire<T> {
    Read(T),
    Failed(String),
}

impl<T> From<std::result::Result<T, String>> for ReadWire<T> {
    fn from(read: std::result::Result<T, String>) -> Self {
        match read {
            Ok(records) => Self::Read(records),
            Err(why) => Self::Failed(why),
        }
    }
}

/// The home, as a client sees it: every read a home-scoped view makes.
///
/// The last two are the App-level state that reaches a home subscriber as
/// an update and nothing else - a subject's snapshot has to cover every
/// update that subject receives, or a late subscriber sees a smaller world
/// than one that attached at boot.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HomeWire {
    pub projects: Vec<ProjectView>,
    pub agents: Vec<AgentRow>,
    pub accounts: AccountsWire,
    pub plugins: PluginsWire,
    pub workers: Vec<WorkersWire>,
    pub connectors: ConnectorsWire,
    pub dictate: DictateWire,
    /// The claude CLI versions the core holds, as the core's own snapshot
    /// serialises. Its crate is one this one may not name.
    pub cli_version: Option<Value>,
    pub service_status: Option<Value>,
    /// The last fatal error, held by the core: an App-level event with no
    /// state behind it reaches only whoever was subscribed when it fired.
    pub fatal_error: Option<Value>,
}

/// The plugin inventory and its update records.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginsWire {
    pub update_records: Vec<Value>,
}

/// One project's live workers, keyed by the project they belong to.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkersWire {
    pub project: String,
    pub workers: Vec<WorkerWire>,
}

/// One live worker, minus the handles a client can never use.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerWire {
    pub label: String,
    pub charter: String,
    pub slot: SessionSlot,
    pub session_id: Option<forge_primitives::SessionId>,
    pub spawned_at: std::time::SystemTime,
    pub spawned_by: SessionSlot,
    pub needs_tag: bool,
    pub is_git_repo_at_spawn: bool,
}

impl From<&WorkerEntry> for WorkerWire {
    fn from(entry: &WorkerEntry) -> Self {
        Self {
            label: entry.label.clone(),
            charter: entry.charter.clone(),
            slot: entry.slot.clone(),
            session_id: entry.session_id.clone(),
            spawned_at: entry.spawned_at,
            spawned_by: entry.spawned_by.clone(),
            needs_tag: entry.needs_tag,
            is_git_repo_at_spawn: entry.is_git_repo_at_spawn,
        }
    }
}

/// The account pool's state.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountsWire {
    pub loading: Vec<AccountLoadingRow>,
    pub all_loaded: bool,
    pub gateway: GatewayWire,
    pub usage: Vec<AccountUsageWire>,
    pub orgs: Vec<GatewayOrgView>,
}

/// The inference listener's bind state.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GatewayWire {
    pub ready: bool,
    pub port: u16,
    pub bind_error: Option<String>,
}

/// One account's cached usage snapshot, absent until the poller succeeds.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountUsageWire {
    pub display_name: String,
    pub snapshot: Option<Value>,
}

/// What the inbound connectors are watching.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConnectorsWire {
    pub gotify: GotifyWire,
    pub slack: SlackWire,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GotifyWire {
    pub connected: bool,
    pub subscriptions: Vec<GotifySubscription>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SlackWire {
    /// Workspace label to whether that workspace's pump is live, as a list
    /// of pairs: JSON objects with dynamic keys are awkward for a client to
    /// iterate, and the order is the store's.
    pub connected_workspaces: Vec<(String, bool)>,
    pub load_failed: bool,
    pub subscriptions: Vec<SlackSubscription>,
}

/// Dictation's preflight state. The device catalog is absent for the same
/// reason it is absent from the read: enumerating devices is a blocking walk
/// that trips a microphone check, so it is asked for on demand.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DictateWire {
    pub snapshot: Value,
    pub models_dir: Option<std::path::PathBuf>,
}

/// One session, as a client sees it: every read a session-scoped view makes.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionWire {
    pub slot: SessionSlot,
    pub state: SessionStateWire,
    pub header: SessionHeaderWire,
    pub mcp: Option<McpServers>,
    pub processes: Option<ProcessSnapshot>,
    pub monitors: Vec<MonitorRecord>,
    pub pending_ask: Option<PendingAskWire>,
    pub conversation: ConversationWire,
    pub slash_commands: Vec<AvailableCommand>,
    pub subagents: Vec<AvailableAgent>,
    pub file_index: FileIndex,
    pub reviews: ReviewsWire,
    /// The working tree behind the git section. The diff itself is a second,
    /// heavier read and is deliberately out of scope.
    pub work: WorkState,
}

/// Where a session's reads find their own working tree.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionStateWire {
    pub slot: SessionSlot,
    pub scan_cwd: std::path::PathBuf,
}

/// The session's header facts, including the two a client cannot otherwise
/// reach: the catalogue a picker draws its rows from, and whether a turn is
/// in flight.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionHeaderWire {
    pub model: Option<Value>,
    pub effort: Value,
    pub permission_mode: Option<Value>,
    pub context: Value,
    pub available_models: Vec<Value>,
    pub turn_in_flight: bool,
}

/// What a seat is held on: all three kinds of parked interaction, not the
/// two the dock happened to draw first. Each request crosses as the core's
/// own shape, because a client draws it rather than re-deriving it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum PendingAskWire {
    Permission(Value),
    Question(Value),
    SlackDraft(Value),
}

impl From<&PendingAsk> for PendingAskWire {
    fn from(ask: &PendingAsk) -> Self {
        let encode = |body: Result<Value, serde_json::Error>| body.unwrap_or(Value::Null);
        match ask {
            PendingAsk::Permission(request) => {
                Self::Permission(encode(serde_json::to_value(request.as_ref())))
            }
            PendingAsk::Question(request) => {
                Self::Question(encode(serde_json::to_value(request.as_ref())))
            }
            PendingAsk::SlackDraft(draft) => {
                Self::SlackDraft(encode(serde_json::to_value(draft.as_ref())))
            }
        }
    }
}

/// The conversation a session's transcript replayed, and how many times it
/// has compacted. The frames cross as the CLI's own messages, which is what
/// the fold reads on both sides.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConversationWire {
    pub messages: Vec<Value>,
    pub compaction_count: u32,
}

/// The review reads for one branch, each carrying its own answer.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReviewsWire {
    pub threads: ReadWire<Vec<ReviewThread>>,
    pub reviews: ReadWire<Vec<ReviewSet>>,
}

/// A subject's wire form. The ONE place it is produced.
///
/// Async because a session's git section is a filesystem read, so the
/// working tree's state is awaited rather than guessed at.
///
/// A seat forge holds no session for is an error rather than an empty
/// snapshot: `Roster::cwd_for` answers `None` for a project nobody has
/// started, and a client drawing an empty page would read that as a broken
/// one rather than as a seat that does not exist.
pub async fn encode_subject(state: &TransportState, subject: &Subject) -> Result<Value> {
    let surface = &state.surface;
    match subject {
        Subject::Home => Ok(serde_json::to_value(home(surface))?),
        Subject::Session(slot) => {
            let roster = surface.roster();
            let Some(cwd) = roster.cwd_for(slot) else {
                anyhow::bail!("forge holds no session for {slot:?}");
            };
            Ok(serde_json::to_value(session(state, surface, slot, &cwd).await?)?)
        }
    }
}

/// The home's record, from the reads a home-scoped view makes.
fn home(surface: &ViewSurface) -> HomeWire {
    let roster = surface.roster();
    let accounts = surface.accounts();
    let connectors = surface.connectors(None);
    let dictate = surface.dictate();
    let workers = surface.workers();
    let encode = |value: Option<Value>| value;

    HomeWire {
        workers: roster
            .projects
            .iter()
            .map(|project| WorkersWire {
                project: project.name.clone(),
                workers: workers.for_project(&project.key).iter().map(WorkerWire::from).collect(),
            })
            .collect(),
        accounts: AccountsWire {
            loading: accounts.loading,
            all_loaded: accounts.all_loaded,
            gateway: GatewayWire {
                ready: accounts.gateway.ready,
                port: accounts.gateway.port,
                bind_error: accounts.gateway.bind_error,
            },
            usage: accounts
                .usage
                .into_iter()
                .map(|row| AccountUsageWire {
                    display_name: row.display_name,
                    snapshot: row.snapshot.and_then(|snapshot| serde_json::to_value(snapshot).ok()),
                })
                .collect(),
            orgs: accounts.orgs,
        },
        plugins: PluginsWire {
            update_records: surface
                .plugins()
                .update_records
                .iter()
                .filter_map(|record| serde_json::to_value(record).ok())
                .collect(),
        },
        connectors: ConnectorsWire {
            gotify: GotifyWire {
                connected: connectors.gotify.connected,
                subscriptions: connectors.gotify.subscriptions,
            },
            slack: SlackWire {
                connected_workspaces: connectors.slack.connected_workspaces.into_iter().collect(),
                load_failed: connectors.slack.load_failed,
                subscriptions: connectors.slack.subscriptions,
            },
        },
        dictate: DictateWire {
            snapshot: serde_json::to_value(dictate.snapshot).unwrap_or(Value::Null),
            models_dir: dictate.models_dir,
        },
        cli_version: surface.cli_version().and_then(|version| serde_json::to_value(version).ok()),
        service_status: surface.service_status().and_then(|issue| serde_json::to_value(issue).ok()),
        fatal_error: encode(
            surface.fatal_error().and_then(|error| serde_json::to_value(error).ok()),
        ),
        agents: surface.agents().all().to_vec(),
        projects: roster.projects,
    }
}

/// One session's record, from the reads a session-scoped view makes.
async fn session(
    state: &TransportState,
    surface: &ViewSurface,
    slot: &SessionSlot,
    cwd: &Path,
) -> Result<SessionWire> {
    let header = surface.header(slot);
    let conversation = surface.conversation(slot, cwd);
    let work = state.work.snapshot(slot, cwd).await;
    let branch = work.branch.clone().unwrap_or_default();
    let reviews = surface.reviews(slot.project(), &branch);
    let state_at = surface.session(slot, cwd);

    Ok(SessionWire {
        slot: slot.clone(),
        file_index: surface.file_index(&state_at.scan_cwd),
        slash_commands: surface.slash_commands(slot),
        subagents: surface.subagents(slot),
        mcp: surface.mcp_servers(slot),
        processes: surface.processes(slot),
        monitors: surface.monitors(slot),
        pending_ask: surface.pending_ask(slot).as_ref().map(PendingAskWire::from),
        conversation: ConversationWire {
            messages: conversation
                .messages
                .iter()
                .filter_map(|message| serde_json::to_value(message).ok())
                .collect(),
            compaction_count: conversation.compaction_count,
        },
        header: SessionHeaderWire {
            model: header.model.as_ref().and_then(|model| serde_json::to_value(model).ok()),
            effort: serde_json::to_value(header.effort).unwrap_or(Value::Null),
            permission_mode: header
                .permission_mode
                .as_ref()
                .and_then(|mode| serde_json::to_value(mode).ok()),
            context: serde_json::to_value(header.context).unwrap_or(Value::Null),
            available_models: header
                .available_models
                .iter()
                .filter_map(|model| serde_json::to_value(model).ok())
                .collect(),
            turn_in_flight: header.turn_in_flight,
        },
        reviews: ReviewsWire {
            threads: ReadWire::from(reviews.threads),
            reviews: ReadWire::from(reviews.reviews),
        },
        state: SessionStateWire { slot: state_at.slot, scan_cwd: state_at.scan_cwd },
        work,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::surface::PendingKind;
    use crate::work::WorkCache;

    /// Where the fixture fleet is built. Fixed rather than per-run, so the
    /// fixture does not pin one machine's temp directory.
    const FIXTURE_ROOT: &str = "/tmp/forge-wire-fixture";

    /// The surface the fixtures are produced from, and both halves of the
    /// reason are deliberate.
    ///
    /// DETERMINISTIC, because the wire carries absolute paths and a fleet
    /// built in a per-run directory would pin the directory rather than the
    /// shape. POPULATED, because an empty fleet pins almost nothing: most of
    /// a session's fields would be `null`, and a field renamed to `null`
    /// would still pass.
    fn fixture_state() -> TransportState {
        let root = Path::new(FIXTURE_ROOT);
        let _ = std::fs::remove_dir_all(root);
        let fleet =
            crate::testing::Fleet::in_dir(root, &[("TestOrg", &["proj"])]).expect("the fleet");
        fleet.start("TestOrg", "proj").expect("the project starts");
        fleet.set_cli_version(Some("1.0.0"), Some("1.1.0"));
        fleet.set_user_preferences(serde_json::json!({}));
        fleet.add_worker("TestOrg", "proj", "w1").expect("the worker is added");
        fleet.install_agent("TestOrg", "proj", "lead");
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &[
                    r#"{"type":"user","message":{"role":"user","content":"hello"},"session_id":"s"}"#,
                    r#"{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
                ],
            )
            .expect("the transcript seeds");
        fleet.seed_test_pending_interaction(&fixture_seat(), PendingKind::Permission);

        TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
        }
    }

    /// The substrings a fixture must not pin: the directory the fleet
    /// happens to live in, in every form the wire spells it.
    ///
    /// Three forms, because the wire carries all three. A path arrives raw
    /// (`ProjectView::path`), canonically (anything that resolved a symlink)
    /// and SLUGGED (`ProjectKey`, every non-alphanumeric turned into `-`).
    /// The slug form is why replacing the plain path alone is not enough:
    /// the key is a temp directory spelled with dashes, and it differs per
    /// machine just as much as the path does.
    fn volatile(root: &Path) -> Vec<String> {
        let raw = root.to_string_lossy().into_owned();
        let canonical = root
            .canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let mut forms = vec![raw, canonical];
        forms.extend(forms.iter().map(|form| slug(form)).collect::<Vec<_>>());
        forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
        forms.dedup();
        forms
    }

    /// The slug `ProjectKey` builds: every non-alphanumeric becomes `-`.
    fn slug(path: &str) -> String {
        path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
    }

    fn normalise(value: &mut Value, forms: &[String]) {
        match value {
            Value::String(text) => {
                for form in forms {
                    if let Some(rest) = text.strip_prefix(form.as_str()) {
                        *text = format!("<fixture>{rest}");
                        break;
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| normalise(item, forms)),
            Value::Object(fields) => fields.values_mut().for_each(|field| normalise(field, forms)),
            _ => {}
        }
    }

    /// Every subject a client can watch, with the fixture that pins its wire
    /// shape.
    fn fixtures() -> Vec<(Subject, PathBuf)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire_fixtures");
        vec![
            (Subject::Home, dir.join("home.json")),
            (Subject::Session(fixture_seat()), dir.join("session.json")),
        ]
    }

    /// Writes each subject's fixture from the encoder. Run deliberately:
    /// `cargo nextest run -p forge-server --run-ignored ignored-only write_the_fixtures`.
    ///
    /// **What this writes is only what the encoder currently emits**, so a
    /// fixture produced here pins the present shape and a wrong one equally.
    /// Each has to be READ against the intended shape before it is
    /// committed; generating one is not the work.
    #[tokio::test]
    #[ignore = "writes the fixtures; run deliberately"]
    async fn write_the_fixtures() {
        let state = fixture_state();
        let forms = volatile(Path::new(FIXTURE_ROOT));
        std::fs::create_dir_all(fixtures()[0].1.parent().expect("a directory")).expect("mkdir");
        for (subject, fixture) in fixtures() {
            let mut encoded = encode_subject(&state, &subject).await.expect("encode");
            normalise(&mut encoded, &forms);
            std::fs::write(&fixture, serde_json::to_string_pretty(&encoded).expect("render"))
                .expect("write");
        }
    }

    #[tokio::test]
    async fn every_subject_round_trips_through_its_fixture() {
        let state = fixture_state();
        let forms = volatile(Path::new(FIXTURE_ROOT));

        for (subject, fixture) in fixtures() {
            let mut encoded = encode_subject(&state, &subject).await.expect("encode");
            normalise(&mut encoded, &forms);
            let expected: Value =
                serde_json::from_str(&std::fs::read_to_string(&fixture).expect("read"))
                    .expect("parse");
            assert_eq!(encoded, expected, "{} changed shape", fixture.display());
        }
    }
}
