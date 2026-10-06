//! `GotifyFacade` - the seam between the Gotify MCP tools and workspace
//! state. The production impl ([`ProdGotifyFacade`]) resolves the
//! caller's project + durable identity and drives the direct
//! `Workspace` subscription methods; the mock records calls for tool
//! tests.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_connectors::gotify::{GotifyRecent, refresh_app_index};
use forge_primitives::GotifySubscription;
use uuid::Uuid;

use crate::SessionSlot;
use crate::gotify::SubsystemHost;
use crate::mcp::caller_context::caller_context;
use crate::workspace::Workspace;

/// Why `gotify__subscribe` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GotifySubscribeError {
    /// No `[gotify]` block in forge.toml - the server is unconfigured.
    NotConfigured,
    /// The caller couldn't be mapped to a project (transient race, or
    /// the session isn't attached to a forge.toml project).
    UnknownCallerProject,
}

/// A created subscription's effective filter, plus whether that filter can
/// resolve a message. `names_resolve` is false only when the filter names
/// applications and the `/application` lookup behind them failed: the
/// subscription is live, but those names will not match until the next
/// stream reconnect. Always true for a match-any filter, which needs no
/// names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubscribeOutcome {
    pub id: Uuid,
    /// The app names the subscription matches; empty matches any app.
    pub applications: Vec<String>,
    /// The priority floor, `None` for any.
    pub min_priority: Option<u8>,
    pub names_resolve: bool,
}

/// Why a read-only Gotify tool (`gotify__apps` / `gotify__recent`) failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GotifyReadError {
    /// No `[gotify]` block in forge.toml - the server is unconfigured.
    NotConfigured,
    /// The REST request to the server failed (network, HTTP status, or
    /// body parse). Carries the formatted error chain for the LLM.
    Fetch(String),
}

/// The Gotify tools' view of the workspace. The subscription mutations
/// are synchronous state writes; the read-only `apps` / `recent` lookups
/// hit the server's REST API and are async.
#[async_trait::async_trait]
pub(crate) trait GotifyFacade: Send + Sync {
    /// Subscribe the caller's durable identity to the configured server,
    /// optionally filtered by application names and/or minimum priority.
    /// An empty `applications` matches any app. Persists when the identity
    /// is durable. Async because a filter that names applications refreshes
    /// the app index before returning.
    async fn subscribe(
        &self,
        caller: &SessionSlot,
        applications: Vec<String>,
        min_priority: Option<u8>,
    ) -> Result<SubscribeOutcome, GotifySubscribeError>;

    /// The caller's own subscriptions in its project - a lead's, or one
    /// worker's, never another owner's.
    fn list(&self, caller: &SessionSlot) -> Vec<GotifySubscription>;

    /// Remove one of the caller's OWN subscriptions by id within its
    /// project, returning the row as it stood. `None` both when no such
    /// id exists and when it belongs to another owner.
    fn unsubscribe(&self, caller: &SessionSlot, id: Uuid) -> Option<GotifySubscription>;

    /// The application NAMEs on the configured server (`GET /application`)
    /// so a session can self-discover what it may subscribe to.
    async fn apps(&self) -> Result<Vec<String>, GotifyReadError>;

    /// The most recent notifications, newest first, filtered by
    /// application NAME (empty = any) and `min_priority` (`None` = any),
    /// capped at `limit`. A catch-up read for a woken or live session.
    async fn recent(
        &self,
        applications: Vec<String>,
        min_priority: Option<u8>,
        limit: usize,
    ) -> Result<Vec<GotifyRecent>, GotifyReadError>;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdGotifyFacade {
    workspace: Weak<Workspace>,
}

impl ProdGotifyFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn GotifyFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

#[async_trait::async_trait]
impl GotifyFacade for ProdGotifyFacade {
    async fn subscribe(
        &self,
        caller: &SessionSlot,
        applications: Vec<String>,
        min_priority: Option<u8>,
    ) -> Result<SubscribeOutcome, GotifySubscribeError> {
        let ws = self.workspace.upgrade().ok_or(GotifySubscribeError::UnknownCallerProject)?;
        let cfg = ws.gotify_config().ok_or(GotifySubscribeError::NotConfigured)?;
        let (project, team_role, durable) =
            resolve_identity(&ws, caller).ok_or(GotifySubscribeError::UnknownCallerProject)?;
        let sub = GotifySubscription {
            id: Uuid::new_v4(),
            project,
            team_role,
            applications,
            min_priority,
            created_at: SystemTime::now(),
        };
        let names_applications = !sub.applications.is_empty();
        let id = sub.id;
        let applications = sub.applications.clone();
        ws.add_gotify_subscription(sub, durable);
        // The pump starts before the lookup: it must not wait behind a
        // network round trip, and no window may sit between the
        // subscription and the pump that runs it.
        ws.start_gotify_subsystem();
        // An application the server created after the stream connected is
        // absent from the cached index, so a filter naming it matches
        // nothing until the next reconnect.
        let names_resolve = if names_applications {
            refresh_app_index(&SubsystemHost::new(&ws), &cfg).await
        } else {
            true
        };
        Ok(SubscribeOutcome { id, applications, min_priority, names_resolve })
    }

    fn list(&self, caller: &SessionSlot) -> Vec<GotifySubscription> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        // Symmetric with `cron__list`: every caller sees only its own
        // subscriptions - a lead (`worker_label == None`) the lead ones,
        // a worker its own.
        ws.gotify_subscriptions_for_project(&cx.project_name)
            .into_iter()
            .filter(|s| s.team_role == cx.worker_label)
            .collect()
    }

    fn unsubscribe(&self, caller: &SessionSlot, id: Uuid) -> Option<GotifySubscription> {
        let ws = self.workspace.upgrade()?;
        let cx = caller_context(&ws, caller)?;
        let removed = ws.remove_gotify_subscription_owned_by(
            &cx.project_name,
            id,
            cx.worker_label.as_deref(),
        );
        ws.stop_gotify_subsystem_if_idle();
        removed
    }

    async fn apps(&self) -> Result<Vec<String>, GotifyReadError> {
        let ws = self.workspace.upgrade().ok_or(GotifyReadError::NotConfigured)?;
        let cfg = ws.gotify_config().ok_or(GotifyReadError::NotConfigured)?;
        forge_connectors::gotify::app_names(&SubsystemHost::new(&ws), &cfg)
            .await
            .map_err(|err| GotifyReadError::Fetch(format!("{err:#}")))
    }

    async fn recent(
        &self,
        applications: Vec<String>,
        min_priority: Option<u8>,
        limit: usize,
    ) -> Result<Vec<GotifyRecent>, GotifyReadError> {
        let ws = self.workspace.upgrade().ok_or(GotifyReadError::NotConfigured)?;
        let cfg = ws.gotify_config().ok_or(GotifyReadError::NotConfigured)?;
        forge_connectors::gotify::recent_messages(
            &SubsystemHost::new(&ws),
            &cfg,
            &applications,
            min_priority,
            limit,
        )
        .await
        .map_err(|err| GotifyReadError::Fetch(format!("{err:#}")))
    }
}

/// Resolve a caller to `(project_name, team_role, durable)`. `team_role`
/// is the worker's role label (`None` targets the lead); `durable` is
/// true for the lead or a worker with a persisted row in the session
/// store, false for a worker without one.
pub(crate) fn resolve_identity(
    ws: &Workspace,
    caller: &SessionSlot,
) -> Option<(String, Option<String>, bool)> {
    let cx = caller_context(ws, caller)?;
    let worker_label = if cx.is_lead {
        None
    } else {
        ws.list_live_workers(&cx.project_key)
            .into_iter()
            .find(|w| w.slot == *caller)
            .map(|w| w.label)
    };
    let dynamic_labels: Vec<String> =
        ws.worker_rows_for_project(&cx.project_key).into_iter().map(|w| w.label).collect();
    let (team_role, durable) = durable_identity(worker_label.as_deref(), &dynamic_labels);
    Some((cx.project_name, team_role, durable))
}

/// `(team_role, durable)` for a caller's worker label (`None` = the lead
/// or a plain catalog session). A lead is always durable and targets
/// itself. A worker is durable when its label holds a persisted row in
/// the session store, since that row is what brings it back.
fn durable_identity(
    worker_label: Option<&str>,
    dynamic_labels: &[String],
) -> (Option<String>, bool) {
    match worker_label {
        None => (None, true),
        Some(label) => {
            let durable = dynamic_labels.iter().any(|t| t == label);
            (Some(label.to_owned()), durable)
        }
    }
}

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
/// One recorded `subscribe` call: `(caller, applications, min_priority)`.
#[cfg(test)]
type SubscribeCall = (SessionSlot, Vec<String>, Option<u8>);

/// One recorded `recent` call: `(applications, min_priority, limit)`.
#[cfg(test)]
type RecentCall = (Vec<String>, Option<u8>, usize);

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockGotifyFacade {
    pub subs: parking_lot::Mutex<Vec<GotifySubscription>>,
    pub subscribe_calls: parking_lot::Mutex<Vec<SubscribeCall>>,
    pub subscribe_result:
        parking_lot::Mutex<Option<Result<SubscribeOutcome, GotifySubscribeError>>>,
    pub unsubscribe_calls: parking_lot::Mutex<Vec<(SessionSlot, Uuid)>>,
    pub unsubscribe_result: parking_lot::Mutex<Option<GotifySubscription>>,
    pub apps_result: parking_lot::Mutex<Option<Result<Vec<String>, GotifyReadError>>>,
    pub recent_calls: parking_lot::Mutex<Vec<RecentCall>>,
    pub recent_result: parking_lot::Mutex<Option<Result<Vec<GotifyRecent>, GotifyReadError>>>,
}

#[cfg(test)]
impl MockGotifyFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn into_arc(self) -> Arc<dyn GotifyFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl GotifyFacade for MockGotifyFacade {
    async fn subscribe(
        &self,
        caller: &SessionSlot,
        applications: Vec<String>,
        min_priority: Option<u8>,
    ) -> Result<SubscribeOutcome, GotifySubscribeError> {
        self.subscribe_calls.lock().push((caller.clone(), applications.clone(), min_priority));
        self.subscribe_result.lock().clone().unwrap_or_else(|| {
            Ok(SubscribeOutcome {
                id: Uuid::nil(),
                applications,
                min_priority,
                names_resolve: true,
            })
        })
    }

    fn list(&self, _caller: &SessionSlot) -> Vec<GotifySubscription> {
        self.subs.lock().clone()
    }

    fn unsubscribe(&self, caller: &SessionSlot, id: Uuid) -> Option<GotifySubscription> {
        self.unsubscribe_calls.lock().push((caller.clone(), id));
        self.unsubscribe_result.lock().clone()
    }

    async fn apps(&self) -> Result<Vec<String>, GotifyReadError> {
        self.apps_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn recent(
        &self,
        applications: Vec<String>,
        min_priority: Option<u8>,
        limit: usize,
    ) -> Result<Vec<GotifyRecent>, GotifyReadError> {
        self.recent_calls.lock().push((applications, min_priority, limit));
        self.recent_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkerEntry;
    use crate::target::ProjectKey;
    use forge_connectors::gotify::GotifyHost as _;
    use forge_primitives::WorkerLiveness;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A throwaway HTTP server answering the Gotify `/application` lookup,
    /// counting how often it was asked. Every other path 404s, so the
    /// pump's stream upgrade fails fast instead of hanging the test.
    struct AppStub {
        addr: std::net::SocketAddr,
        application_fetches: Arc<AtomicUsize>,
    }

    impl AppStub {
        fn start(status: &'static str, body: &'static str) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind app stub");
            let addr = listener.local_addr().expect("app stub addr");
            let application_fetches = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&application_fetches);
            std::thread::spawn(move || {
                for socket in listener.incoming() {
                    let Ok(mut socket) = socket else { continue };
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        match std::io::Read::read(&mut socket, &mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => raw.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let head = String::from_utf8_lossy(&raw);
                    let (status, body) = if head.starts_with("GET /application ") {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (status, body)
                    } else {
                        ("404 Not Found", "{}")
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = std::io::Write::write_all(&mut socket, response.as_bytes());
                }
            });
            Self { addr, application_fetches }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn application_fetches(&self) -> usize {
            self.application_fetches.load(Ordering::SeqCst)
        }
    }

    /// A workspace carrying a real `[gotify]` config pointed at `stub`,
    /// plus the caller's project, so `ProdGotifyFacade::subscribe` runs
    /// its whole path against the real connector.
    fn subscribe_fixture(
        apps_status: &'static str,
        apps_body: &'static str,
    ) -> (tempfile::TempDir, Arc<Workspace>, Arc<dyn GotifyFacade>, SessionSlot, AppStub) {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = AppStub::start(apps_status, apps_body);
        let forge = crate::config::ensure_forge_data_dir(dir.path()).expect("forge/ dir");
        std::fs::write(
            forge.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]

[[orgs.projects]]
name = "myproj"
path = "/tmp/gotify-subscribe-refresh"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"

[gotify]
url = "{}"
client_token = "Ctest"
"#,
                stub.url(),
            ),
        )
        .expect("write forge.toml");
        let config = crate::config::load_from_dir(dir.path()).expect("load the fixture config");
        let (ws, _rx) =
            Workspace::testing_stub_with_config(dir.path().to_owned(), config).expect("stub");
        let facade = ProdGotifyFacade::from_arc(&ws);
        (dir, ws, facade, SessionSlot::lead("TestOrg", "myproj"), stub)
    }

    /// The reported flow: an application created after the stream
    /// connected is absent from the cached index, so a subscription
    /// naming it silently matches nothing until the next reconnect.
    /// Subscribe must land the index that resolves it.
    #[tokio::test]
    async fn subscribe_refreshes_the_app_index_so_a_new_application_matches() {
        let (_dir, ws, facade, caller, stub) = subscribe_fixture(
            "200 OK",
            r#"[{"id":7,"name":"phone-agent","token":"A.x","description":"","image":""}]"#,
        );
        // The cached index is what a connect-time fetch left behind: it
        // predates the application the caller is about to name.
        let host = SubsystemHost::new(&ws);
        host.store_app_index(HashMap::new());

        let outcome = facade
            .subscribe(&caller, vec!["phone-agent".to_owned()], None)
            .await
            .expect("subscribe to the configured server");

        assert!(outcome.names_resolve, "a refreshed index leaves the name filter resolvable");
        assert_eq!(
            outcome.applications,
            ["phone-agent"],
            "the outcome carries the filter that was stored, not an empty default",
        );
        assert_eq!(outcome.min_priority, None, "and the priority floor it registered");
        assert_eq!(stub.application_fetches(), 1, "the subscribe refreshed the index");
        let resolved = host.app_name(7);
        let subs = ws.gotify_subscriptions_for_project("myproj");
        let matched =
            forge_connectors::gotify::matching_subscriptions(&subs, resolved.as_deref(), 5);
        assert_eq!(
            matched.len(),
            1,
            "a message from the newly created application matches its subscription: resolved={resolved:?}",
        );
    }

    /// A filter that names no application matches anything, so it needs no
    /// names to resolve and must not pay for a lookup.
    #[tokio::test]
    async fn subscribe_without_application_names_does_not_fetch_the_index() {
        let (_dir, _ws, facade, caller, stub) = subscribe_fixture("200 OK", "[]");

        let outcome = facade
            .subscribe(&caller, Vec::new(), Some(5))
            .await
            .expect("subscribe without filters");

        assert!(outcome.names_resolve, "a match-any filter needs no names, so it always resolves");
        assert_eq!(
            stub.application_fetches(),
            0,
            "a match-any subscribe names no applications, so it must not fetch /application",
        );
    }

    /// The failing refresh is the one case the reply can speak to: without
    /// it, a subscribe that reports success and matches nothing is exactly
    /// what #1298 was filed for.
    #[tokio::test]
    async fn subscribe_reports_a_failed_index_refresh_and_keeps_the_subscription() {
        let (_dir, ws, facade, caller, _stub) =
            subscribe_fixture("500 Internal Server Error", "{}");

        let outcome = facade
            .subscribe(&caller, vec!["phone-agent".to_owned()], None)
            .await
            .expect("the subscription itself still succeeds");

        assert!(
            !outcome.names_resolve,
            "an unrefreshable index leaves the name filter unresolvable, and that is reported",
        );
        assert_eq!(
            ws.gotify_subscriptions_for_project("myproj").len(),
            1,
            "the subscription is created whether or not the index refreshed",
        );
    }

    fn worker_entry(project: &str, label: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "watch".to_owned(),
            slot: SessionSlot::worker("TestOrg", project, label),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", project),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// A project with a live lead plus two live workers, mirroring the
    /// cron facade's fixture so the two families are tested the same way.
    fn fixture() -> (Arc<Workspace>, Arc<dyn GotifyFacade>, ProjectKey, SessionSlot, SessionSlot) {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/gotify-scope");
        let key =
            ws.list_projects().into_iter().find(|v| v.name == "myproj").expect("seeded view").key;
        ws.record_connected_session("/tmp/gotify-scope", "lead-uuid", None);
        ws.insert_live_worker(&key, worker_entry("myproj", "reviewer"));
        ws.insert_live_worker(&key, worker_entry("myproj", "analyst"));
        let facade = ProdGotifyFacade::from_arc(&ws);
        (
            ws,
            facade,
            key,
            SessionSlot::lead("TestOrg", "myproj"),
            SessionSlot::worker("TestOrg", "myproj", "reviewer"),
        )
    }

    fn seed_sub(ws: &Workspace, team_role: Option<&str>) -> Uuid {
        let sub = GotifySubscription {
            id: Uuid::new_v4(),
            project: "myproj".to_owned(),
            team_role: team_role.map(str::to_owned),
            applications: vec!["alerts".to_owned()],
            min_priority: None,
            created_at: SystemTime::UNIX_EPOCH,
        };
        let id = sub.id;
        ws.add_gotify_subscription(sub, true);
        id
    }

    #[test]
    fn list_is_scoped_to_the_callers_own_subscriptions() {
        let (ws, facade, _key, lead, worker) = fixture();
        let lead_id = seed_sub(&ws, None);
        let worker_id = seed_sub(&ws, Some("reviewer"));
        seed_sub(&ws, Some("analyst"));

        let worker_list = facade.list(&worker);
        assert_eq!(
            worker_list.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![worker_id],
            "a worker sees neither the lead's subscription nor a sibling's",
        );
        let lead_list = facade.list(&lead);
        assert_eq!(
            lead_list.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![lead_id],
            "a lead sees only its own subscriptions",
        );
    }

    #[test]
    fn unsubscribe_only_removes_the_callers_own_subscription() {
        let (ws, facade, _key, lead, worker) = fixture();
        let lead_id = seed_sub(&ws, None);
        let worker_id = seed_sub(&ws, Some("reviewer"));
        let sibling_id = seed_sub(&ws, Some("analyst"));

        assert!(
            facade.unsubscribe(&worker, lead_id).is_none(),
            "a worker cannot unsubscribe the lead's",
        );
        assert!(
            facade.unsubscribe(&worker, sibling_id).is_none(),
            "a worker cannot unsubscribe a sibling worker's",
        );
        assert!(
            facade.unsubscribe(&lead, worker_id).is_none(),
            "a lead cannot unsubscribe a worker's",
        );
        assert_eq!(
            ws.gotify_subscriptions_for_project("myproj").len(),
            3,
            "a refused unsubscribe removes nothing",
        );

        let removed = facade
            .unsubscribe(&worker, worker_id)
            .expect("a worker unsubscribes its own, and gets the row back");
        assert_eq!(removed.id, worker_id, "the echoed row is the one that went");
        assert_eq!(removed.applications, ["alerts"], "carrying its effective filter");
        assert!(facade.unsubscribe(&lead, lead_id).is_some(), "a lead unsubscribes its own");
        assert_eq!(
            ws.gotify_subscriptions_for_project("myproj").iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![sibling_id],
            "only the untouched sibling subscription remains",
        );
    }

    /// Ownership is enforced at the MCP facade, NOT in the workspace
    /// teardown method, so a despawn still clears a departing worker's
    /// subscriptions with no MCP caller involved.
    #[test]
    fn worker_teardown_clears_subscriptions_without_going_through_the_facade() {
        let (ws, _facade, key, _lead, _worker) = fixture();
        let lead_id = seed_sub(&ws, None);
        let worker_id = seed_sub(&ws, Some("reviewer"));

        ws.remove_gotify_subscriptions_for_worker(&key, "reviewer");

        let remaining: Vec<Uuid> =
            ws.gotify_subscriptions_for_project("myproj").iter().map(|s| s.id).collect();
        assert!(!remaining.contains(&worker_id), "teardown removed the departing worker's");
        assert!(remaining.contains(&lead_id), "the lead's subscription survives the despawn");
    }

    #[test]
    fn worker_is_durable_when_persisted() {
        let dynamic = vec!["scratch".to_owned()];
        assert_eq!(durable_identity(Some("scratch"), &dynamic), (Some("scratch".to_owned()), true),);
    }

    #[test]
    fn lead_is_durable_and_targets_itself() {
        assert_eq!(durable_identity(None, &[]), (None, true));
    }

    #[test]
    fn worker_without_a_row_is_ephemeral() {
        // No row means nothing re-spawns the label, so its subscription is
        // in-memory only rather than written to redb for an absent owner.
        assert_eq!(
            durable_identity(Some("scratch"), &["reviewer".to_owned()]),
            (Some("scratch".to_owned()), false),
        );
    }
}
