//! The Slack facade's host wiring: the `SlackFacadeHost` implementation, the
//! caller/project resolution it owns, and the paths a tool call takes through
//! a real workspace, a real subsystem and a recording Slack client.
//!
//! The facade itself lives in `forge_tools::slack::facade`; these tests stay
//! here because what they exercise is the host half.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::SystemTime;

use forge_primitives::SessionSlot;
use forge_primitives::slack::{
    SlackBookmark, SlackConfig, SlackConversation, SlackDraftAnswer, SlackDraftEnding,
    SlackDraftOperation, SlackFile, SlackFileShare, SlackFileShares, SlackPerson, SlackPin,
    SlackPinMessage, SlackSearchMatch, SlackSubscription, SlackSubscriptionTarget, SlackUser,
    SlackUserProfile, SlackWatchMode,
};
use forge_tools::slack::facade::{
    ProdSlackFacade, SlackAttachmentError, SlackChannelWatch, SlackEditError, SlackEditRequest,
    SlackFacade, SlackFacadeHost, SlackFetchRequest, SlackPostError, SlackPostRequest,
    SlackReactError, SlackReactRequest, SlackReadError, SlackSubscribeRequest,
    SlackUnsubscribeError, SlackUploadRequest, SlackWorkspaceError, format_workspace_error,
};
use forge_tools::slack::{
    AuthTest, DraftResolution, MENTION_CURSOR, MessagePage, PostedMessage, SearchPage, SlackApi,
    SlackError, SlackHistoryMessage,
};
use tempfile::tempdir;
use uuid::Uuid;

use crate::config::LoadedConfig;
use crate::mcp::test_support::text_of;
use crate::workspace::Workspace;

fn caller() -> SessionSlot {
    SessionSlot::lead("TestOrg", "forge")
}

/// The production facade over one workspace: the subsystem handle and the
/// host wiring, both weak, as the spawn path builds it.
fn prod_facade(ws: &Arc<Workspace>) -> Arc<dyn SlackFacade> {
    let host: Arc<dyn SlackFacadeHost> = ws.clone();
    Arc::new(ProdSlackFacade::new(Arc::downgrade(&ws.slack), Arc::downgrade(&host)))
}

/// One recorded history read: conversation, oldest, newest, limit.
type HistoryRead = (String, Option<String>, Option<String>, u32);

/// Records every outbound call, so a test can assert what did and did
/// not reach Slack.
#[derive(Default)]
struct RecordingApi {
    posts: parking_lot::Mutex<Vec<(String, String, Option<String>)>>,
    updates: parking_lot::Mutex<Vec<(String, String, String)>>,
    deletes: parking_lot::Mutex<Vec<(String, String)>>,
    reactions: parking_lot::Mutex<Vec<(String, String, String, bool)>>,
    /// Messages Slack refuses to change, by ts: not the user's own.
    foreign: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// Seeded files: id -> (name as the uploader chose it, bytes).
    files: parking_lot::Mutex<HashMap<String, (String, Vec<u8>)>>,
    uploads: parking_lot::Mutex<Vec<(String, String, Option<String>)>>,
    upload_urls: parking_lot::Mutex<usize>,
    /// Seeded reads: channel -> rows.
    pins: parking_lot::Mutex<HashMap<String, Vec<SlackPin>>>,
    bookmark_rows: parking_lot::Mutex<HashMap<String, Vec<SlackBookmark>>>,
    pin_reads: parking_lot::Mutex<Vec<String>>,
    bookmark_reads: parking_lot::Mutex<Vec<String>>,
    /// Seeded conversation and thread reads.
    history_rows: parking_lot::Mutex<HashMap<String, Vec<SlackHistoryMessage>>>,
    /// Make `conversations.list` fail, so the draft's fallback label is
    /// the one tested.
    list_failure: parking_lot::Mutex<bool>,
    reply_rows: parking_lot::Mutex<HashMap<String, Vec<SlackHistoryMessage>>>,
    /// Thread reads that fail, keyed `channel/ts`.
    reply_failures: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// User ids whose lookup fails, so a tag stays unnamed.
    user_failures: parking_lot::Mutex<std::collections::HashSet<String>>,
    history_reads: parking_lot::Mutex<Vec<HistoryRead>>,
    reply_reads: parking_lot::Mutex<Vec<(String, String)>>,
    /// Search answers page by page; the queries are recorded so a test
    /// can prove the conversation name was narrowed into `in:`.
    search_pages: parking_lot::Mutex<Vec<Vec<SlackSearchMatch>>>,
    search_reads: parking_lot::Mutex<Vec<String>>,
    /// The directory a user resolver reads.
    users_rows: parking_lot::Mutex<Vec<SlackUser>>,
    /// The ts `chat.postMessage` answers with.
    post_ts: parking_lot::Mutex<Option<String>>,
    /// The channel `chat.postMessage` answers with; defaults to the
    /// channel the request named.
    post_channel: parking_lot::Mutex<Option<String>>,
    /// Answer no channel at all, the shape Slack omits it in.
    post_channel_absent: parking_lot::Mutex<bool>,
    /// Parks a post, an upload or a `files.info` until the test
    /// releases it.
    post_park: parking_lot::Mutex<Option<Arc<tokio::sync::Notify>>>,
    upload_park: parking_lot::Mutex<Option<Arc<tokio::sync::Notify>>>,
    file_info_park: parking_lot::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// How many posts have been answered, so an unseeded answer is a
    /// distinct ts per call - a multi-part post needs two.
    post_calls: parking_lot::Mutex<usize>,
    /// The 0-based post call that fails, for a partial multi-part post.
    post_fail_at: parking_lot::Mutex<Option<usize>>,
    /// Where each seeded file was shared: id -> shares.
    file_shares: parking_lot::Mutex<HashMap<String, SlackFileShares>>,
    /// The shares `files.completeUploadExternal` answers with, when a
    /// test wants the extra `files.info` read to be skipped.
    upload_shares: parking_lot::Mutex<Option<SlackFileShares>>,
    /// How many `files.info` calls were made.
    file_info_calls: parking_lot::Mutex<usize>,
    /// Seeded conversations, for the display name a held draft carries.
    conversations: parking_lot::Mutex<Vec<SlackConversation>>,
}

impl RecordingApi {
    fn refuse_edits_of(&self, ts: &str) {
        self.foreign.lock().insert(ts.to_owned());
    }
    fn seed_file(&self, id: &str, name: &str, bytes: &[u8]) {
        self.files.lock().insert(id.to_owned(), (name.to_owned(), bytes.to_vec()));
    }
    fn seed_pins(&self, channel: &str, rows: Vec<SlackPin>) {
        self.pins.lock().insert(channel.to_owned(), rows);
    }
    fn seed_bookmarks(&self, channel: &str, rows: Vec<SlackBookmark>) {
        self.bookmark_rows.lock().insert(channel.to_owned(), rows);
    }
    fn seed_history(&self, channel: &str, rows: Vec<SlackHistoryMessage>) {
        self.history_rows.lock().insert(channel.to_owned(), rows);
    }
    fn fail_list(&self) {
        *self.list_failure.lock() = true;
    }
    fn seed_replies(&self, channel: &str, ts: &str, rows: Vec<SlackHistoryMessage>) {
        self.reply_rows.lock().insert(format!("{channel}/{ts}"), rows);
    }
    fn fail_replies(&self, channel: &str, ts: &str) {
        self.reply_failures.lock().insert(format!("{channel}/{ts}"));
    }
    fn fail_user_info(&self, user: &str) {
        self.user_failures.lock().insert(user.to_owned());
    }
    fn seed_search(&self, rows: Vec<SlackSearchMatch>) {
        self.search_pages.lock().push(rows);
    }
    fn seed_search_pages(&self, pages: Vec<Vec<SlackSearchMatch>>) {
        self.search_pages.lock().extend(pages);
    }
    fn seed_users(&self, rows: Vec<SlackUser>) {
        self.users_rows.lock().extend(rows);
    }
    fn history_reads(&self) -> Vec<HistoryRead> {
        self.history_reads.lock().clone()
    }
    fn reply_reads(&self) -> Vec<(String, String)> {
        self.reply_reads.lock().clone()
    }
    fn search_reads(&self) -> Vec<String> {
        self.search_reads.lock().clone()
    }
    fn posts(&self) -> Vec<(String, String, Option<String>)> {
        self.posts.lock().clone()
    }
    fn upload_url_count(&self) -> usize {
        *self.upload_urls.lock()
    }
    fn uploads(&self) -> Vec<(String, String, Option<String>)> {
        self.uploads.lock().clone()
    }
    fn seed_conversation(&self, id: &str, name: &str) {
        self.conversations.lock().push(SlackConversation {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            is_channel: true,
            is_private: false,
            is_im: false,
            is_mpim: false,
            is_archived: false,
            user: None,
            purpose: None,
            topic: None,
        });
    }
    fn seed_dm(&self, id: &str, user: &str) {
        self.conversations.lock().push(SlackConversation {
            id: id.to_owned(),
            name: None,
            is_channel: false,
            is_private: false,
            is_im: true,
            is_mpim: false,
            is_archived: false,
            user: Some(user.to_owned()),
            purpose: None,
            topic: None,
        });
    }
    fn seed_mpim(&self, id: &str, name: &str) {
        self.conversations.lock().push(SlackConversation {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            is_channel: false,
            is_private: false,
            is_im: false,
            is_mpim: true,
            is_archived: false,
            user: None,
            purpose: None,
            topic: None,
        });
    }
    fn answer_posts_with(&self, ts: &str) {
        *self.post_ts.lock() = Some(ts.to_owned());
    }
    /// The channel `chat.postMessage` answers with.
    fn answer_posts_in(&self, channel: &str) {
        *self.post_channel.lock() = Some(channel.to_owned());
    }
    /// Answer no channel at all, the shape Slack omits it in.
    fn answer_posts_without_channel(&self) {
        *self.post_channel_absent.lock() = true;
    }
    /// Park every post until the returned gate is notified.
    fn park_posts(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.post_park.lock() = Some(Arc::clone(&gate));
        gate
    }
    /// Park every upload until the returned gate is notified.
    fn park_uploads(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.upload_park.lock() = Some(Arc::clone(&gate));
        gate
    }
    /// Park every `files.info` until the returned gate is notified.
    fn park_file_info(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.file_info_park.lock() = Some(Arc::clone(&gate));
        gate
    }
    /// Make the 0-based `index` post call fail.
    fn fail_post_at(&self, index: usize) {
        *self.post_fail_at.lock() = Some(index);
    }
    /// Seed where a file is shared, as `files.info` reports it: the
    /// message ts for the conversation the share names.
    fn seed_file_shares(&self, id: &str, conversation: &str, ts: &str) {
        let mut shares = SlackFileShares::default();
        shares.public.insert(conversation.to_owned(), vec![SlackFileShare { ts: ts.to_owned() }]);
        self.file_shares.lock().insert(id.to_owned(), shares);
    }
    /// Seed the shares `files.completeUploadExternal` itself answers
    /// with, so no `files.info` follow-up is needed.
    fn seed_upload_shares(&self, conversation: &str, ts: &str) {
        let mut shares = SlackFileShares::default();
        shares.public.insert(conversation.to_owned(), vec![SlackFileShare { ts: ts.to_owned() }]);
        *self.upload_shares.lock() = Some(shares);
    }
    fn file_info_calls(&self) -> usize {
        *self.file_info_calls.lock()
    }
}

#[async_trait::async_trait]
impl SlackApi for RecordingApi {
    async fn auth_test(&self) -> Result<AuthTest, SlackError> {
        Ok(AuthTest {
            team: "Test".to_owned(),
            user: "tester".to_owned(),
            team_id: "T1".to_owned(),
            user_id: "U1".to_owned(),
            url: "https://test.slack.com/".to_owned(),
        })
    }

    async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
        if *self.list_failure.lock() {
            return Err(SlackError::Transport {
                method: "conversations.list".to_owned(),
                detail: "connection reset".to_owned(),
            });
        }
        Ok(self.conversations.lock().clone())
    }

    async fn history(
        &self,
        channel: &str,
        oldest: Option<&str>,
        newest: Option<&str>,
        limit: u32,
        _cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        self.history_reads.lock().push((
            channel.to_owned(),
            oldest.map(str::to_owned),
            newest.map(str::to_owned),
            limit,
        ));
        Ok(MessagePage {
            messages: self.history_rows.lock().get(channel).cloned().unwrap_or_default(),
            next_cursor: None,
        })
    }

    async fn replies(
        &self,
        channel: &str,
        ts: &str,
        _oldest: Option<&str>,
        _limit: u32,
        _cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        self.reply_reads.lock().push((channel.to_owned(), ts.to_owned()));
        if self.reply_failures.lock().contains(&format!("{channel}/{ts}")) {
            return Err(SlackError::RateLimited {
                method: "conversations.replies".to_owned(),
                retry_after: std::time::Duration::from_secs(5),
            });
        }
        Ok(MessagePage {
            messages: self
                .reply_rows
                .lock()
                .get(&format!("{channel}/{ts}"))
                .cloned()
                .unwrap_or_default(),
            next_cursor: None,
        })
    }

    async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostedMessage, SlackError> {
        self.posts.lock().push((channel.to_owned(), text.to_owned(), thread_ts.map(str::to_owned)));
        let attempt = {
            let mut calls = self.post_calls.lock();
            let attempt = *calls;
            *calls += 1;
            attempt
        };
        let gate = self.post_park.lock().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if *self.post_fail_at.lock() == Some(attempt) {
            return Err(SlackError::Api {
                method: "chat.postMessage".to_owned(),
                error: "rate_limited".to_owned(),
                needed: None,
            });
        }
        let ts = self.post_ts.lock().clone().unwrap_or_else(|| format!("{}.0", attempt + 1));
        let answered = if *self.post_channel_absent.lock() {
            None
        } else {
            Some(self.post_channel.lock().clone().unwrap_or_else(|| channel.to_owned()))
        };
        Ok(PostedMessage { channel: answered, ts })
    }

    async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), SlackError> {
        if self.foreign.lock().contains(ts) {
            return Err(SlackError::Api {
                method: "chat.update".to_owned(),
                error: "cant_update_message".to_owned(),
                needed: None,
            });
        }
        self.updates.lock().push((channel.to_owned(), ts.to_owned(), text.to_owned()));
        Ok(())
    }

    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), SlackError> {
        if self.foreign.lock().contains(ts) {
            return Err(SlackError::Api {
                method: "chat.delete".to_owned(),
                error: "cant_delete_message".to_owned(),
                needed: None,
            });
        }
        self.deletes.lock().push((channel.to_owned(), ts.to_owned()));
        Ok(())
    }

    async fn set_reaction(
        &self,
        channel: &str,
        ts: &str,
        name: &str,
        add: bool,
    ) -> Result<(), SlackError> {
        self.reactions.lock().push((channel.to_owned(), ts.to_owned(), name.to_owned(), add));
        Ok(())
    }

    async fn file_info(&self, id: &str) -> Result<SlackFile, SlackError> {
        *self.file_info_calls.lock() += 1;
        let gate = self.file_info_park.lock().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let files = self.files.lock();
        let Some((name, _)) = files.get(id) else {
            return Err(SlackError::Api {
                method: "files.info".to_owned(),
                error: "file_not_found".to_owned(),
                needed: None,
            });
        };
        Ok(SlackFile {
            id: id.to_owned(),
            name: name.clone(),
            url_private: format!("https://files.test/{id}"),
            shares: self.file_shares.lock().get(id).cloned().unwrap_or_default(),
        })
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SlackError> {
        let id = url.rsplit('/').next().unwrap_or_default();
        self.files.lock().get(id).map(|(_, bytes)| bytes.clone()).ok_or_else(|| {
            SlackError::Transport {
                method: "download".to_owned(),
                detail: format!("no seeded file at {url}"),
            }
        })
    }

    async fn post_bytes(&self, _url: &str, _body: Vec<u8>) -> Result<(), SlackError> {
        Ok(())
    }

    async fn get_upload_url(
        &self,
        name: &str,
        _length: usize,
    ) -> Result<(String, String), SlackError> {
        *self.upload_urls.lock() += 1;
        Ok((format!("https://upload.test/{name}"), "F-uploaded".to_owned()))
    }

    async fn complete_upload(
        &self,
        file_id: &str,
        channel: &str,
        thread_ts: Option<&str>,
    ) -> Result<Vec<SlackFile>, SlackError> {
        self.uploads.lock().push((
            file_id.to_owned(),
            channel.to_owned(),
            thread_ts.map(str::to_owned),
        ));
        let gate = self.upload_park.lock().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(vec![SlackFile {
            id: file_id.to_owned(),
            name: String::new(),
            url_private: String::new(),
            shares: self.upload_shares.lock().clone().unwrap_or_default(),
        }])
    }

    async fn search_messages(
        &self,
        query: &str,
        _count: u32,
        cursor: Option<&str>,
    ) -> Result<SearchPage, SlackError> {
        self.search_reads.lock().push(query.to_owned());
        let pages = self.search_pages.lock();
        let index = cursor.and_then(|cursor| cursor.parse::<usize>().ok()).unwrap_or(0);
        let matches = pages.get(index).cloned().unwrap_or_default();
        Ok(SearchPage {
            matches,
            next_cursor: (index + 1 < pages.len()).then(|| (index + 1).to_string()),
        })
    }

    async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError> {
        if self.user_failures.lock().contains(user) {
            return Err(SlackError::RateLimited {
                method: "users.info".to_owned(),
                retry_after: std::time::Duration::from_secs(5),
            });
        }
        Ok(SlackUser {
            id: user.to_owned(),
            name: "tester".to_owned(),
            real_name: None,
            tz: None,
            deleted: false,
            profile: None,
        })
    }

    async fn list_users(&self) -> Result<Vec<SlackUser>, SlackError> {
        Ok(self.users_rows.lock().clone())
    }

    async fn pins(&self, channel: &str) -> Result<Vec<SlackPin>, SlackError> {
        self.pin_reads.lock().push(channel.to_owned());
        Ok(self.pins.lock().get(channel).cloned().unwrap_or_default())
    }

    async fn bookmarks(&self, channel: &str) -> Result<Vec<SlackBookmark>, SlackError> {
        self.bookmark_reads.lock().push(channel.to_owned());
        Ok(self.bookmark_rows.lock().get(channel).cloned().unwrap_or_default())
    }
}

/// A workspace whose one Slack workspace is the recording double, with
/// a lead session recorded so the caller resolves to a project.
fn facade_with_recording_slack() -> (
    Arc<dyn SlackFacade>,
    Arc<Workspace>,
    Arc<RecordingApi>,
    tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
) {
    let (facade, ws, api, rx, _dir) = facade_with_recording_slack_inner(false);
    (facade, ws, api, rx)
}

/// A recording facade, its workspace, the double, the update stream,
/// and the tempdir that must outlive them.
type RecordingSlack = (
    Arc<dyn SlackFacade>,
    Arc<Workspace>,
    Arc<RecordingApi>,
    tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
    tempfile::TempDir,
);

/// The same workspace with a store installed, for the paths that
/// persist - the own-post ledger.
fn facade_with_recording_slack_and_store() -> RecordingSlack {
    facade_with_recording_slack_inner(true)
}

fn facade_with_recording_slack_inner(with_store: bool) -> RecordingSlack {
    let dir = tempdir().expect("tempdir");
    let mut config = LoadedConfig::empty_for_test();
    config.slack = vec![cfg("acme")];
    let api = Arc::new(RecordingApi::default());
    let mut apis: BTreeMap<String, Arc<dyn SlackApi>> = BTreeMap::new();
    apis.insert("acme".to_owned(), api.clone());
    let (ws, rx) = Workspace::testing_stub_with_slack(
        dir.path().to_path_buf(),
        config,
        Arc::new(forge_tools::slack::SlackWorkspaces::from_apis(apis)),
        None,
    );
    if with_store {
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
    }
    ws.seed_test_project_with_slack_workspace("forge", "/tmp/slack-facade-scope", "acme");
    ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
    let facade = prod_facade(&ws);
    (facade, ws, api, rx, dir)
}

/// The session's slot names the scope, so a tool with no `workspace`
/// argument resolves the project's binding and the result names it -
/// the label stays visible, it just stops being an input.
#[tokio::test]
async fn a_tool_resolves_each_projects_binding_from_the_config_file() {
    // The production path: the bindings come from a real forge.toml, so
    // deleting the config lookup cannot leave this green - unlike the
    // overlay-seeded fixtures the other tests build.
    let dir = tempdir().expect("tempdir");
    let forge = dir.path().join("forge");
    std::fs::create_dir_all(&forge).expect("forge/");
    std::fs::write(
        forge.join("forge.toml"),
        r#"
[[orgs]]
name = "Busytools"
accounts = ["Acct"]
[[orgs.projects]]
name = "alpha"
path = "/tmp/alpha"
[orgs.projects.slack]
workspace = "acme"
[[orgs.projects]]
name = "beta"
path = "/tmp/beta"
[orgs.projects.slack]
workspace = "beta-ws"
[[orgs.projects]]
name = "gamma"
path = "/tmp/gamma"
[[accounts]]
display_name = "Acct"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
[[slack]]
workspace = "acme"
token = "x"
[[slack]]
workspace = "beta-ws"
token = "y"
"#,
    )
    .expect("write forge.toml");
    let config = crate::config::load_from_dir(dir.path()).expect("the config loads");
    let mut apis: BTreeMap<String, Arc<dyn SlackApi>> = BTreeMap::new();
    apis.insert("acme".to_owned(), Arc::new(RecordingApi::default()));
    apis.insert("beta-ws".to_owned(), Arc::new(RecordingApi::default()));
    let (ws, _rx) = Workspace::testing_stub_with_slack(
        dir.path().to_path_buf(),
        config,
        Arc::new(forge_tools::slack::SlackWorkspaces::from_apis(apis)),
        None,
    );
    let facade = prod_facade(&ws);

    for (project, expected) in [("alpha", "acme"), ("beta", "beta-ws")] {
        let tool = forge_tools::slack::SlackTools::new(
            facade.clone(),
            SessionSlot::lead("Busytools", project),
        );
        let out = forge_mcp::ToolProvider::call(&tool, "slack__me", serde_json::json!({}))
            .await
            .expect("the slack family owns slack__me")
            .expect("the tool answers");
        let text = crate::mcp::test_support::text_of(&out);
        assert!(!out.is_error, "{project} answers: {text}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text).expect("identity json")["workspace"],
            expected,
            "the slot's own project binding is the one resolved",
        );
    }

    let unbound =
        forge_tools::slack::SlackTools::new(facade, SessionSlot::lead("Busytools", "gamma"));
    let out = forge_mcp::ToolProvider::call(&unbound, "slack__list", serde_json::json!({}))
        .await
        .expect("the slack family owns slack__list")
        .expect("the tool answers");
    let text = crate::mcp::test_support::text_of(&out);
    assert!(out.is_error, "a project with no binding refuses: {text}");
    assert!(text.contains("Busytools/gamma"), "names the scope: {text}");
    assert!(text.contains("[orgs.slack]"), "names the org key: {text}");
    assert!(text.contains("[orgs.projects.slack]"), "names the project key: {text}");
}

/// An answer as the registry takes it. Every call site here is a view
/// answering; the drop is a dead waiter, and it goes through the paths
/// under test rather than through this.
fn answered(approved: bool) -> DraftResolution {
    DraftResolution::Answered(if approved {
        SlackDraftAnswer::Approve
    } else {
        SlackDraftAnswer::Decline
    })
}

fn sent_back(note: &str) -> DraftResolution {
    DraftResolution::Answered(SlackDraftAnswer::SendBack { note: note.to_owned() })
}

/// Wait for the blocked post to register its draft, then hand back its
/// id. The post is parked on a oneshot until this is resolved.
async fn wait_for_draft(ws: &Arc<Workspace>) -> Uuid {
    for _ in 0..200 {
        if let Some(id) = ws.slack.held_drafts().first().map(|(_, draft)| draft.id) {
            return id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("no draft was registered");
}

/// Poll until `check` holds, failing the test rather than hanging.
async fn until(check: impl Fn() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the condition never held");
}

fn post_request(conversation: &str, text: &str) -> SlackPostRequest {
    SlackPostRequest {
        conversation: conversation.to_owned(),
        thread_ts: None,
        text: text.to_owned(),
    }
}

fn edit_request(conversation: &str, ts: &str, text: Option<&str>) -> SlackEditRequest {
    SlackEditRequest {
        conversation: conversation.to_owned(),
        ts: ts.to_owned(),
        text: text.map(str::to_owned),
    }
}

fn react_request(conversation: &str, ts: &str, name: &str, add: bool) -> SlackReactRequest {
    SlackReactRequest {
        conversation: conversation.to_owned(),
        ts: ts.to_owned(),
        name: name.to_owned(),
        add,
    }
}

fn fetch_request(file_id: &str, dir: &std::path::Path) -> SlackFetchRequest {
    SlackFetchRequest { file_id: file_id.to_owned(), dir: dir.to_path_buf() }
}

fn upload_request(conversation: &str, path: &std::path::Path) -> SlackUploadRequest {
    SlackUploadRequest {
        conversation: conversation.to_owned(),
        thread_ts: None,
        path: path.to_path_buf(),
        title: None,
    }
}

#[tokio::test]
async fn an_attachment_lands_on_disk_with_the_expected_bytes() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_file("F1", "notes.txt", b"attachment body");
    let dir = tempdir().expect("tempdir");

    let path =
        facade.fetch_attachment(&caller(), fetch_request("F1", dir.path())).await.expect("fetched");
    assert_eq!(std::fs::read(&path).expect("read back"), b"attachment body");
    assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("notes.txt"));
}

#[tokio::test]
async fn a_download_refuses_to_escape_the_chosen_directory() {
    // The name comes from Slack, so it is attacker-controlled input as
    // far as this code is concerned.
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_file("F1", "../../escape.txt", b"nope");
    let dir = tempdir().expect("tempdir");

    let path =
        facade.fetch_attachment(&caller(), fetch_request("F1", dir.path())).await.expect("fetched");
    // `starts_with` is component-wise, so `dir/../../escape.txt` passes
    // it lexically while resolving above `dir`. Assert the parent
    // instead: the file lands IN the directory or the test is asleep.
    assert_eq!(
        path.parent(),
        Some(dir.path()),
        "the file must land directly in the chosen directory, never above it: {path:?}",
    );
}

/// The name comes from Slack and the directory is not exclusive to
/// this call, so a fetch must never clobber something already there.
#[tokio::test]
async fn a_download_refuses_to_clobber_an_existing_file() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_file("F1", "notes.txt", b"new bytes");
    let dir = tempdir().expect("tempdir");
    let existing = dir.path().join("notes.txt");
    std::fs::write(&existing, b"precious").expect("write the existing file");

    assert!(
        facade.fetch_attachment(&caller(), fetch_request("F1", dir.path())).await.is_err(),
        "an existing file must not be overwritten",
    );
    assert_eq!(
        std::fs::read(&existing).expect("read back"),
        b"precious",
        "and the existing file is untouched",
    );
}

/// A fetch into a directory that does not exist yet just works: the
/// directory is created, however many levels are missing.
#[tokio::test]
async fn a_fetch_creates_a_missing_target_directory() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_file("F1", "notes.txt", b"attachment body");
    let root = tempdir().expect("tempdir");
    let dir = root.path().join("fresh/deeper");

    let path =
        facade.fetch_attachment(&caller(), fetch_request("F1", &dir)).await.expect("fetched");
    assert_eq!(
        path.parent(),
        Some(dir.as_path()),
        "the file lands in the directory that was created: {path:?}",
    );
    assert_eq!(std::fs::read(&path).expect("read back"), b"attachment body");
}

/// A `dir` that exists but is a file is refused with the typed error
/// before Slack is contacted: `F1` is unseeded here, so an error other
/// than `NotADirectory` means the fetch reached Slack first.
#[tokio::test]
async fn a_fetch_rejects_a_dir_that_is_a_file() {
    let (facade, _ws, _api, _rx) = facade_with_recording_slack();
    let root = tempdir().expect("tempdir");
    let not_a_dir = root.path().join("plain-file");
    std::fs::write(&not_a_dir, b"data").expect("write the file");

    let err = facade
        .fetch_attachment(&caller(), fetch_request("F1", &not_a_dir))
        .await
        .expect_err("a file is not a directory");
    assert_eq!(
        err,
        SlackAttachmentError::NotADirectory(not_a_dir.clone()),
        "the error carries the path the caller chose",
    );
}

/// When the write still fails after the checks, the error names the
/// directory the caller chose instead of a bare errno.
#[tokio::test]
async fn a_fetch_failure_names_the_path() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_file("F1", "notes.txt", b"body");
    let root = tempdir().expect("tempdir");
    let block = root.path().join("plain-file");
    std::fs::write(&block, b"data").expect("write the file");
    let dir = block.join("sub");

    let err = facade
        .fetch_attachment(&caller(), fetch_request("F1", &dir))
        .await
        .expect_err("cannot land");
    let SlackAttachmentError::Io(message) = err else {
        panic!("expected the io error, got {err:?}");
    };
    assert!(
        message.contains(&dir.display().to_string()),
        "the error names the directory: {message}",
    );
}

/// The approval prompt shows the conversation. The id is not a name
/// anybody recognises, so it is resolved before the draft is held.
#[tokio::test]
async fn a_held_draft_names_the_conversation_not_its_id() {
    let (facade, ws, api, mut rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });

    let mut seen = None;
    for _ in 0..200 {
        while let Ok(frame) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::SlackPostPending { draft, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                seen = Some(draft.conversation_label);
            }
        }
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        seen.expect("the draft reaches the prompt"),
        "#ops",
        "the prompt names the channel the message goes to, as a channel",
    );

    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// The draft the facade has parked, which is the copy the pending update
/// carries.
async fn parked_draft(ws: &Arc<Workspace>) -> forge_primitives::slack::SlackDraft {
    for _ in 0..200 {
        let draft = ws.slack.held_drafts().into_iter().next().map(|(_, draft)| draft);
        if let Some(draft) = draft {
            return draft;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("no draft was registered");
}

/// A thread read that answers one parent message.
fn parent_message(ts: &str, user: &str, text: &str) -> SlackHistoryMessage {
    SlackHistoryMessage {
        ts: ts.to_owned(),
        user: Some(user.to_owned()),
        text: text.to_owned(),
        thread_ts: Some(ts.to_owned()),
        reply_count: 0,
        bot_name: None,
        parent_user_id: None,
        latest_reply: None,
        edited: None,
        files: Vec::new(),
    }
}

fn threaded_post(conversation: &str, ts: &str, text: &str) -> SlackPostRequest {
    SlackPostRequest {
        conversation: conversation.to_owned(),
        thread_ts: Some(ts.to_owned()),
        text: text.to_owned(),
    }
}

/// The card names the thread it replies into and the people it tags, so
/// the reader is not shown a ts and raw ids.
#[tokio::test]
async fn a_held_draft_names_its_thread_and_the_people_it_tags() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies("C1", "100.0", vec![parent_message("100.0", "U2", "the deploy\nis green")]);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "confirmed <@U9>")).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(
        draft.thread,
        Some(forge_primitives::slack::SlackThreadName {
            author: "tester".to_owned(),
            excerpt: "the deploy is green".to_owned(),
        }),
        "the thread is named by its author and one flattened line",
    );
    assert_eq!(
        draft.tagged,
        vec![forge_primitives::slack::SlackPerson {
            id: "U9".to_owned(),
            name: Some("tester".to_owned()),
        }],
        "the tagged person is named, not left as an id",
    );

    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A mention inside the quoted parent is named by the same lookup the
/// draft's own tags use, so the line reads as prose rather than markup.
#[tokio::test]
async fn a_parents_mention_reads_as_its_name_in_the_excerpt() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies(
        "C1",
        "100.0",
        vec![parent_message("100.0", "U2", "<@U9> the deploy is green")],
    );

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    assert_eq!(
        parked_draft(&ws).await.thread.map(|thread| thread.excerpt),
        Some("@tester the deploy is green".to_owned()),
        "the parent's mention draws its name, not the raw id",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A mention the lookup cannot name keeps its id, exactly as a tag does,
/// so nothing is invented.
#[tokio::test]
async fn a_parents_mention_keeps_its_id_when_the_lookup_fails() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.fail_user_info("U9");
    api.seed_replies(
        "C1",
        "100.0",
        vec![parent_message("100.0", "U2", "<@U9> the deploy is green")],
    );

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    assert_eq!(
        parked_draft(&ws).await.thread.map(|thread| thread.excerpt),
        Some("U9 the deploy is green".to_owned()),
        "an unnamed mention keeps its id rather than a guessed name",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// Only the draft's own text tags people: a mention in the quoted parent
/// is named in the excerpt but is not added to `tagged`.
#[tokio::test]
async fn only_the_drafts_own_mentions_are_tagged() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies(
        "C1",
        "100.0",
        vec![parent_message("100.0", "U2", "<@U8> the deploy is green")],
    );

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello <@U9>")).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(
        draft.tagged,
        vec![SlackPerson { id: "U9".to_owned(), name: Some("tester".to_owned()) }],
        "a parent's mention is named in the excerpt, not tagged on the draft",
    );
    assert_eq!(
        draft.thread.map(|thread| thread.excerpt),
        Some("@tester the deploy is green".to_owned()),
        "the parent's mention still reads as its name",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// The quote is bounded after its markup is flattened, so the bound is a
/// budget of prose and the cut lands on a word.
#[tokio::test]
async fn a_quoted_parent_is_bounded_after_its_markup_is_flattened() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies(
        "C1",
        "100.0",
        vec![parent_message(
            "100.0",
            "U2",
            "*<https://example.com|the staging deploy>* is <@U9> and every check is green \
                 while the p99 is still high and the canary holds at one percent",
        )],
    );

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    let excerpt = parked_draft(&ws).await.thread.expect("the thread is named").excerpt;
    assert!(excerpt.ends_with("..."), "the flattened line is cut: {excerpt:?}");
    assert!(
        excerpt.chars().count() <= ProdSlackFacade::THREAD_EXCERPT_CHARS + 3,
        "the bound holds after flattening: {excerpt:?}",
    );
    assert!(
        excerpt.chars().count() > ProdSlackFacade::THREAD_EXCERPT_CHARS - 5,
        "the budget counts prose, so markup did not consume the cut: {excerpt:?}",
    );
    assert!(
        !excerpt.contains('<') && !excerpt.contains('*'),
        "the bound is counted on prose, not raw markup: {excerpt:?}",
    );
    let kept = excerpt.strip_suffix("...").expect("the mark is there");
    assert!(!kept.ends_with(' '), "the cut lands at a word boundary: {excerpt:?}");

    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A root post has no thread to name, and its tags still resolve.
#[tokio::test]
async fn a_root_post_names_no_thread_but_still_names_its_tags() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "ping <@U9>")).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(draft.thread, None, "a root message has no thread segment");
    assert_eq!(
        draft.tagged,
        vec![forge_primitives::slack::SlackPerson {
            id: "U9".to_owned(),
            name: Some("tester".to_owned()),
        }],
        "the tags resolve whether or not the message replies into a thread",
    );

    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A parent Slack no longer returns leaves the thread unnamed, and the
/// draft still parks.
#[tokio::test]
async fn a_parent_slack_no_longer_returns_leaves_the_thread_unnamed() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    assert_eq!(parked_draft(&ws).await.thread, None, "no parent means no name to draw");
    assert_eq!(
        api.reply_reads(),
        vec![("C1".to_owned(), "100.0".to_owned())],
        "the read was attempted against the conversation, not skipped",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// The card names a channel the one way, and a DM by its partner: only a
/// channel carries a `#`. A caller echoing the card's own label back
/// still resolves, so a tool result can be fed into the next tool.
#[tokio::test]
async fn a_channel_is_prefixed_and_a_dm_and_a_group_dm_are_not() {
    let recording = Arc::new(RecordingApi::default());
    recording.seed_conversation("C1", "ops");
    recording.seed_dm("D1", "U9");
    recording.seed_mpim("G1", "mpdm-ved-alice-1");
    let api: Arc<dyn SlackApi> = Arc::clone(&recording) as Arc<dyn SlackApi>;

    assert_eq!(
        ProdSlackFacade::resolve_conversation(&api, "acme", "C1").await,
        ("C1".to_owned(), "#ops".to_owned()),
        "a channel's label is the card's spelling",
    );
    assert_eq!(
        ProdSlackFacade::resolve_conversation(&api, "acme", "D1").await,
        ("D1".to_owned(), "U9".to_owned()),
        "a DM is named after its partner, never with a `#`",
    );
    assert_eq!(
        ProdSlackFacade::resolve_conversation(&api, "acme", "G1").await,
        ("G1".to_owned(), "mpdm-ved-alice-1".to_owned()),
        "a group DM keeps Slack's own generated name",
    );
    assert_eq!(
        ProdSlackFacade::resolve_conversation(&api, "acme", "#ops").await,
        ("C1".to_owned(), "#ops".to_owned()),
        "the card's own label resolves when an agent echoes it back",
    );
}

/// When the directory cannot be read, the id stays the bare form Slack
/// can address and the label keeps the caller's own spelling.
#[tokio::test]
async fn a_list_failure_keeps_the_callers_spelling_and_a_bare_id() {
    let recording = Arc::new(RecordingApi::default());
    recording.fail_list();
    let api: Arc<dyn SlackApi> = Arc::clone(&recording) as Arc<dyn SlackApi>;

    assert_eq!(
        ProdSlackFacade::resolve_conversation(&api, "acme", "#ops").await,
        ("ops".to_owned(), "#ops".to_owned()),
        "the id is addressable; the label keeps what the caller said",
    );
    assert_eq!(
        ProdSlackFacade::resolve_conversation(&api, "acme", "U9").await,
        ("U9".to_owned(), "U9".to_owned()),
        "a partner id carries no `#` either way",
    );
}

/// An edit addressed by a DM partner reads the thread from the id Slack
/// knows, exactly as a post does.
#[tokio::test]
async fn an_edit_addressed_by_a_dm_partner_reads_the_resolved_id() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_dm("D1", "U9");
    api.seed_replies("D1", "100.0", vec![parent_message("100.0", "U2", "the message")]);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("U9", "100.0", Some("rewritten"))).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(
        draft.thread.map(|thread| thread.excerpt),
        Some("the message".to_owned()),
        "the edit names the message it acts on",
    );
    assert_eq!(
        api.reply_reads(),
        vec![("D1".to_owned(), "100.0".to_owned())],
        "the read names the conversation Slack knows",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// An upload addressed by a DM partner reads its thread from the
/// resolved id too: the fix covers every path that builds a draft.
#[tokio::test]
async fn an_upload_addressed_by_a_dm_partner_reads_the_resolved_id() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_dm("D1", "U9");
    api.seed_replies("D1", "100.0", vec![parent_message("100.0", "U2", "the parent")]);
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, b"body").expect("write the file");
    let mut request = upload_request("U9", &path);
    request.thread_ts = Some("100.0".to_owned());

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), request).await }
    });

    assert!(parked_draft(&ws).await.thread.is_some(), "the upload names its thread");
    assert_eq!(
        api.reply_reads(),
        vec![("D1".to_owned(), "100.0".to_owned())],
        "the read names the conversation Slack knows",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A parent with no author at all cannot be named without inventing one,
/// so no name is set.
#[tokio::test]
async fn a_parent_with_no_author_leaves_the_thread_unnamed() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies(
        "C1",
        "100.0",
        vec![SlackHistoryMessage {
            user: None,
            bot_name: None,
            ..parent_message("100.0", "U2", "the parent")
        }],
    );

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    assert_eq!(parked_draft(&ws).await.thread, None, "no author means no name to draw");
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// The bound counts characters, not bytes: a parent of two-byte glyphs
/// is cut at the same place a Latin one would be.
#[tokio::test]
async fn a_multibyte_parent_is_cut_by_characters_not_bytes() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    let text = "é".repeat(100);
    api.seed_replies("C1", "100.0", vec![parent_message("100.0", "U2", &text)]);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    let excerpt = parked_draft(&ws).await.thread.expect("the thread is named").excerpt;
    assert_eq!(
        excerpt.chars().count(),
        ProdSlackFacade::THREAD_EXCERPT_CHARS + 3,
        "the bound is characters plus the mark, not bytes",
    );
    assert!(excerpt.starts_with('é'), "the glyph survives whole: {excerpt:?}");
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A read anchored at a reply answers the thread's parent. Naming that
/// would label an edit with a different message, so a ts that is not the
/// one asked for leaves the card drawing the raw stamp.
#[tokio::test]
async fn a_message_inside_a_thread_is_not_named_as_the_thread_parent() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies("C1", "200.0", vec![parent_message("100.0", "U2", "the thread parent")]);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "200.0", Some("rewritten"))).await }
    });

    assert_eq!(
        parked_draft(&ws).await.thread,
        None,
        "the thread parent is not the message a reply's edit acts on",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A draft addressed the way the schema tells agents to address a DM -
/// by the partner's user id - still names its thread: the read uses the
/// resolved conversation id while the wire keeps what the caller named.
#[tokio::test]
async fn a_draft_addressed_by_a_dm_partner_names_the_thread_by_its_resolved_id() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_dm("D1", "U9");
    api.seed_replies("D1", "100.0", vec![parent_message("100.0", "U2", "the parent")]);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("U9", "100.0", "hello")).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(draft.conversation, "U9", "the wire keeps what the caller addressed");
    assert!(draft.thread.is_some(), "the thread is read from the resolved id, not the partner's");
    assert_eq!(
        api.reply_reads(),
        vec![("D1".to_owned(), "100.0".to_owned())],
        "the read names the conversation Slack knows",
    );

    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A top-level message is named for the edit or the delete that acts on
/// it: the read returns the message itself, so its author and opening
/// words are the card's.
#[tokio::test]
async fn an_edit_or_a_delete_of_a_top_level_message_names_it() {
    for text in [Some("rewritten"), None] {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.seed_conversation("C1", "ops");
        api.seed_replies("C1", "100.0", vec![parent_message("100.0", "U2", "the message")]);

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", text)).await }
        });

        let draft = parked_draft(&ws).await;
        assert_eq!(
            draft.thread.map(|thread| thread.excerpt),
            Some("the message".to_owned()),
            "the operation names the message it acts on",
        );
        let id = wait_for_draft(&ws).await;
        ws.slack.resolve_slack_draft(id, &caller(), answered(false));
        let _ = task.await;
    }
}

/// A failed name lookup still names the author by the id Slack sent -
/// never an empty author, which would draw a bare `@`.
#[tokio::test]
async fn a_failed_author_lookup_draws_the_raw_id_as_the_author() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies("C1", "100.0", vec![parent_message("100.0", "U2", "the parent")]);
    api.fail_user_info("U2");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(
        draft.thread.map(|thread| thread.author),
        Some("U2".to_owned()),
        "the raw id is drawn rather than an invented or empty name",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// A bot names itself on the parent, so the card draws that name without
/// spending a users.info call on the bot's id.
#[tokio::test]
async fn a_bot_parent_is_named_by_its_own_name() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_replies(
        "C1",
        "100.0",
        vec![SlackHistoryMessage {
            bot_name: Some("Deploy Bot".to_owned()),
            ..parent_message("100.0", "U3", "the deploy is green")
        }],
    );
    api.fail_user_info("U3");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "hello")).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(
        draft.thread.map(|thread| thread.author),
        Some("Deploy Bot".to_owned()),
        "the bot's own name wins over a lookup",
    );
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// When both lookups fail the draft is still held and drawable: no
/// thread name, and the tag keeps its id instead of being dropped.
#[tokio::test]
async fn a_draft_still_parks_when_the_parent_and_a_tag_lookup_fail() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.fail_replies("C1", "100.0");
    api.fail_user_info("U9");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), threaded_post("C1", "100.0", "confirmed <@U9>")).await }
    });

    let draft = parked_draft(&ws).await;
    assert_eq!(draft.thread, None, "a failed parent read is not invented over");
    assert_eq!(
        draft.tagged,
        vec![forge_primitives::slack::SlackPerson { id: "U9".to_owned(), name: None }],
        "an unresolved tag keeps its id, never dropped",
    );

    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

/// The ts Slack answers a post with is the handle an edit, a delete or
/// a reaction takes, and Slack has already sent it.
#[tokio::test]
async fn a_post_carries_the_ts_slack_answered_with() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.answer_posts_with("1790186552.442169");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    let outcome = task.await.expect("the post task does not panic").expect("the post landed");
    assert_eq!(
        outcome.ts,
        vec!["1790186552.442169".to_owned()],
        "the ts the wire sent reaches the caller",
    );
    assert_eq!(outcome.workspace, "acme", "and the label it posted into rides along");
}

/// Every part Slack accepted is ledgered as it lands. A long draft is
/// several messages, and one that failed halfway would otherwise echo
/// the parts that did post back into the session.
#[tokio::test]
async fn a_multi_part_post_ledgers_every_landed_part() {
    let (facade, ws, _api, _rx, _dir) = facade_with_recording_slack_and_store();
    let text = "a".repeat(forge_tools::slack::POST_LIMIT + 1);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", &text)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    let outcome = task.await.expect("no panic").expect("the parts land");
    assert_eq!(outcome.ts, vec!["1.0".to_owned(), "2.0".to_owned()], "two parts were sent");
    for ts in &outcome.ts {
        assert!(
            ws.slack.slack_own_post("acme", "C1", ts),
            "part {ts} must be refused to the sweep"
        );
    }
    assert!(
        !ws.slack.slack_own_post("acme", "C2", "1.0"),
        "and the ledger stays scoped to the conversation it was posted in",
    );
}

/// A request can address a DM by user id or a channel by name, and
/// Slack answers the conversation it actually posted into. The ledger
/// has to key on that answer, or the sweep looks under the id and the
/// agent's own DM comes back to it.
#[tokio::test]
async fn a_post_is_ledgered_under_the_channel_slack_posted_into() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    api.answer_posts_with("1700000000.000100");
    api.answer_posts_in("D456");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("U123", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    let outcome = task.await.expect("no panic").expect("the post lands");

    assert_eq!(outcome.ts, vec!["1700000000.000100".to_owned()]);
    assert!(
        ws.slack.slack_own_post("acme", "D456", "1700000000.000100"),
        "the conversation the post landed in is refused to the sweep",
    );
    assert!(
        !ws.slack.slack_own_post("acme", "U123", "1700000000.000100"),
        "the address it was sent to is not the conversation the sweep reads",
    );
}

/// A port that records ts but not the answer's channel would false the
/// assertion above; the same guard for the partial case is that the
/// parts Slack took are recorded even when a later part fails.
#[tokio::test]
async fn a_partial_multi_part_post_ledgers_the_parts_slack_took() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    api.fail_post_at(1);
    let text = "a".repeat(forge_tools::slack::POST_LIMIT + 1);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", &text)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    let outcome = task.await.expect("no panic");

    assert!(
        matches!(outcome, Err(SlackPostError::Partial { posted: 1, .. })),
        "the failure names how much landed: {outcome:?}",
    );
    assert!(
        ws.slack.slack_own_post("acme", "C1", "1.0"),
        "the part Slack took is refused to the sweep",
    );
}

/// The mark is held across the post call: without it a sweep in the
/// window delivers what the post is about to ledger.
#[tokio::test]
async fn a_post_marks_its_conversation_in_flight_until_the_ledger_write() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    let gate = api.park_posts();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    let mut marked = false;
    for _ in 0..400 {
        if ws.slack.slack_post_in_flight("acme", "C1").is_some() {
            marked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(marked, "the facade marks the conversation before the post returns");

    gate.notify_one();
    task.await.expect("no panic").expect("the post lands");
    assert!(
        ws.slack.slack_post_in_flight("acme", "C1").is_none(),
        "and clears it once the ts is ledgered",
    );
}

/// Each part carries its own mark: a part that has burned its budget
/// must not leave the next one unprotected, which is what a stamp
/// taken once per operation does.
#[tokio::test]
async fn each_post_part_takes_a_fresh_in_flight_mark() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    let gate = api.park_posts();
    let text = "a".repeat(forge_tools::slack::POST_LIMIT + 1);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", &text)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    until(|| ws.slack.slack_post_in_flight("acme", "C1").is_some()).await;
    // As if the first part had used its whole request budget.
    ws.slack.expire_post_marks("acme", "C1");
    gate.notify_one();

    // Part two is parked once its call has been recorded; its mark
    // must be fresh, not the expired one.
    until(|| api.posts().len() == 2).await;
    assert!(
        ws.slack.slack_post_in_flight("acme", "C1").is_some(),
        "the second part takes a fresh mark of its own",
    );
    gate.notify_one();
    task.await.expect("no panic").expect("both parts land");
}

/// A failed post leaves no mark behind: the guard drops on the error
/// return, so the conversation is swept again next tick.
#[tokio::test]
async fn a_failed_post_clears_its_in_flight_mark() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    api.fail_post_at(0);

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    assert!(task.await.expect("no panic").is_err(), "the post fails");
    assert!(
        ws.slack.slack_post_in_flight("acme", "C1").is_none(),
        "a failed post leaves no mark behind",
    );
}

/// The upload site takes the same mark: it is held until the shares
/// have been read and ledgered.
#[tokio::test]
async fn an_upload_marks_its_conversation_in_flight_until_the_ledger_write() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    let gate = api.park_uploads();
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    let mut marked = false;
    for _ in 0..400 {
        if ws.slack.slack_post_in_flight("acme", "C1").is_some() {
            marked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(marked, "the facade marks the conversation before the upload completes");

    gate.notify_one();
    task.await.expect("no panic").expect("an approved upload sends");
    assert!(
        ws.slack.slack_post_in_flight("acme", "C1").is_none(),
        "and clears it once the upload's shares are ledgered",
    );
}

/// The shares read after the upload carries a mark of its own: the
/// file message is already in history while that call is in flight,
/// and its ts is not ledgered until it returns.
#[tokio::test]
async fn the_upload_marks_its_conversation_while_the_shares_are_read() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    api.seed_file("F-uploaded", "file.txt", b"payload");
    let gate = api.park_file_info();
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    until(|| api.file_info_calls() >= 1).await;
    assert!(
        ws.slack.slack_post_in_flight("acme", "C1").is_some(),
        "the shares read holds a fresh mark of its own",
    );

    gate.notify_one();
    task.await.expect("no panic").expect("the upload sends");
    assert!(
        ws.slack.slack_post_in_flight("acme", "C1").is_none(),
        "and clears it once the shares are ledgered",
    );
}

/// A share read that fails is a ledger gap, not a failed upload: the
/// file is already posted, and reporting it as unsent would invite a
/// duplicate.
#[tokio::test]
async fn an_upload_whose_shares_cannot_be_read_still_succeeds() {
    let (facade, ws, _api, _rx, _dir) = facade_with_recording_slack_and_store();
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    assert!(
        task.await.expect("no panic").is_ok(),
        "an unreadable share is a ledger gap, not a failed upload",
    );
    assert!(
        ws.slack.slack_post_in_flight("acme", "C1").is_none(),
        "and the mark is cleared whatever the share read did",
    );
}

/// Slack's `chat.postMessage` answers the channel it posted into; the
/// fallback is for the case it omits it, and it is right where it can
/// be right: a conversation addressed by id.
#[tokio::test]
async fn a_post_answered_without_a_channel_is_ledgered_under_the_resolved_conversation() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    api.answer_posts_with("1700000000.000100");
    api.answer_posts_without_channel();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("the post lands");

    assert!(
        ws.slack.slack_own_post("acme", "C1", "1700000000.000100"),
        "the resolved conversation is what the fallback keys on",
    );
}

/// An attachment posts a message of its own, and the upload call
/// answers no message ts - the file's shares do. The share's own
/// conversation is what the ledger keys on.
#[tokio::test]
async fn an_upload_ledgers_the_message_under_the_shares_own_conversation() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    api.seed_file("F-uploaded", "file.txt", b"payload");
    api.seed_file_shares("F-uploaded", "D456", "1700000000.000100");
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("U123", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("an approved upload sends");

    assert!(
        ws.slack.slack_own_post("acme", "D456", "1700000000.000100"),
        "the share's own conversation holds the message, so that is what the ledger keys",
    );
    assert!(
        !ws.slack.slack_own_post("acme", "U123", "1700000000.000100"),
        "the request string is not a conversation",
    );
}

/// When the upload's own answer already carries the shares, the
/// `files.info` follow-up is not worth a call.
#[tokio::test]
async fn an_upload_answer_that_carries_its_shares_costs_no_files_info_call() {
    let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
    api.seed_upload_shares("C1", "1700000000.000100");
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("an approved upload sends");

    assert_eq!(api.file_info_calls(), 0, "the answer was enough; no follow-up read");
    assert!(
        ws.slack.slack_own_post("acme", "C1", "1700000000.000100"),
        "and the message it named is refused to the sweep",
    );
}

/// An edit keeps the message's ts, so the rewritten message is refused
/// to the sweep whether forge posted it or the user did.
#[tokio::test]
async fn an_edit_ledgers_the_message_ts_it_rewrote() {
    let (facade, ws, _api, _rx, _dir) = facade_with_recording_slack_and_store();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("an approved replacement is sent");

    assert!(
        ws.slack.slack_own_post("acme", "C1", "100.0"),
        "the edited message's own ts is refused to the sweep",
    );
}

/// The draft text is what the prompt shows. A file NAME is
/// caller-controlled and can look innocuous, so approving it without
/// the local path it comes from approves something the user has not
/// seen.
#[tokio::test]
async fn an_upload_draft_names_the_local_path_it_will_send() {
    let (facade, ws, _api, mut rx) = facade_with_recording_slack();
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        let source = source.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });

    let mut seen = None;
    for _ in 0..200 {
        while let Ok(frame) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::SlackPostPending { draft, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                seen = Some(draft.text);
            }
        }
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let text = seen.expect("the draft reaches the prompt");
    assert!(text.contains("file.txt"), "the file name is shown: {text}");
    assert!(
        text.contains(&source.display().to_string()),
        "and so is the local path it comes from: {text}",
    );

    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));
    let _ = task.await;
}

#[tokio::test]
async fn an_upload_goes_through_the_gate() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));

    assert!(task.await.expect("no panic").is_err(), "a rejected upload must not send");
    assert_eq!(api.upload_url_count(), 0, "nothing is requested from Slack either");
}

#[tokio::test]
async fn an_approved_upload_posts_the_bytes_then_completes() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("an approved upload sends");

    assert_eq!(api.upload_url_count(), 1, "the upload URL is requested once");
    assert_eq!(api.uploads().len(), 1, "and completed once");
    assert_eq!(api.uploads()[0].1, "C1", "into the conversation that was named");
}

/// The waiter going away is its own ending: nothing was answered, and a
/// view still holding the dock hears it rather than silence.
#[tokio::test]
async fn a_waiter_that_dies_is_reported_as_abandoned() {
    let (facade, ws, _api, mut rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let _id = wait_for_draft(&ws).await;

    task.abort();
    let _ = task.await;

    let mut resolved = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        if let crate::protocol::SessionUpdate::SlackDraftResolved { ending, .. } =
            std::sync::Arc::unwrap_or_clone(frame.update)
        {
            resolved.push(ending);
        }
    }
    assert_eq!(
        resolved,
        vec![SlackDraftEnding::Abandoned],
        "a dropped waiter clears the registry and says which ending took the draft",
    );
    assert!(ws.slack.held_drafts().is_empty(), "and leaves nothing registered");
}

#[tokio::test]
async fn a_rejected_draft_never_reaches_slack() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));

    let outcome = task.await.expect("no panic");
    assert_eq!(outcome, Err(SlackPostError::Rejected));
    assert!(api.posts().is_empty(), "a rejected draft must not post");
}

/// A send-back is not a decline: the blocked call reads the reader's
/// words and revises from them, and nothing goes out.
#[tokio::test]
async fn a_sent_back_post_hands_the_note_back_and_sends_nothing() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), sent_back("say staging, not prod"));

    let outcome = task.await.expect("no panic");
    assert_eq!(
        outcome,
        Err(SlackPostError::SentBack { note: "say staging, not prod".to_owned() }),
        "the note reaches the blocked call whole",
    );
    assert!(api.posts().is_empty(), "a sent-back draft must not post");
}

/// Each gated operation has its own error, so each has its own sent-back
/// arm: one arm left as a plain decline would send the agent guessing.
#[tokio::test]
async fn a_sent_back_edit_hands_the_note_back_and_leaves_the_message() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new"))).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), sent_back("keep the original wording"));

    assert_eq!(
        task.await.expect("no panic"),
        Err(SlackEditError::SentBack { note: "keep the original wording".to_owned() }),
    );
    assert!(api.updates.lock().is_empty(), "a sent-back replacement is never sent");
}

#[tokio::test]
async fn a_sent_back_delete_hands_the_note_back_and_keeps_the_message() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", None)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), sent_back("that one needs to stay"));

    assert_eq!(
        task.await.expect("no panic"),
        Err(SlackEditError::SentBack { note: "that one needs to stay".to_owned() }),
    );
    assert!(api.deletes.lock().is_empty(), "a sent-back deletion never removes anything");
}

#[tokio::test]
async fn a_sent_back_reaction_hands_the_note_back_and_applies_nothing() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.react(&caller(), react_request("C1", "100.0", "thumbsup", true)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), sent_back("that reads as sarcasm here"));

    assert_eq!(
        task.await.expect("no panic"),
        Err(SlackReactError::SentBack { note: "that reads as sarcasm here".to_owned() }),
    );
    assert!(api.reactions.lock().is_empty(), "a sent-back reaction is never applied");
}

#[tokio::test]
async fn a_sent_back_upload_hands_the_note_back_and_sends_no_bytes() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("file.txt");
    std::fs::write(&source, b"payload").expect("write the source file");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), sent_back("wrong file, send the report instead"));

    assert_eq!(
        task.await.expect("no panic"),
        Err(SlackAttachmentError::SentBack {
            note: "wrong file, send the report instead".to_owned(),
        }),
    );
    assert_eq!(api.upload_url_count(), 0, "a sent-back upload never reaches Slack");
}

/// A note belongs to the attempt it answered. The revised call parks its
/// own hold, and nothing sends until the reader approves that one.
#[tokio::test]
async fn a_revised_call_after_a_send_back_parks_its_own_draft() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let first = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "prod is down")).await }
    });
    let first_id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(first_id, &caller(), sent_back("say staging, not prod"));
    assert!(first.await.expect("no panic").is_err(), "the first attempt was sent back");

    let second = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "staging is down")).await }
    });
    let second_id = wait_for_draft(&ws).await;

    assert_ne!(second_id, first_id, "the revision parks its own card");
    assert!(api.posts().is_empty(), "and nothing approves the revision on its behalf");

    ws.slack.resolve_slack_draft(second_id, &caller(), answered(true));
    second.await.expect("no panic").expect("the reader's approval is what sends it");
    assert_eq!(api.posts().len(), 1, "one message goes out");
    assert_eq!(api.posts()[0].1, "staging is down", "the revised words are what went out");
}

/// A prompt nobody answers must read as "not approved". Dropping the
/// sender is how that happens - the session went away, or the dock
/// prompt was cleared without a decision - and it must never be taken
/// for an approval.
#[tokio::test]
async fn a_draft_whose_answer_never_comes_posts_nothing() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    let _ =
        ws.slack.resolve_slack_draft(id, &caller(), forge_tools::slack::DraftResolution::Abandoned);

    let outcome = task.await.expect("no panic");
    assert_eq!(outcome, Err(SlackPostError::Rejected));
    assert!(api.posts().is_empty(), "an unanswered draft must not post");
}

/// A held draft waits for the user, not a clock. Advancing a day leaves
/// the call parked, and the answer it later gets still posts - the
/// behaviour a question, a permission and a hand-off already have.
#[tokio::test(start_paused = true)]
async fn a_held_draft_waits_for_the_user_rather_than_a_clock() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;

    tokio::time::advance(std::time::Duration::from_secs(24 * 60 * 60)).await;
    assert!(!task.is_finished(), "a held draft waits for the user, not a clock");

    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    let outcome = task.await.expect("no panic");
    assert!(outcome.is_ok(), "the answer still posts after a day of waiting: {outcome:?}");
    assert_eq!(api.posts().len(), 1, "the held message goes out once");
}

/// The outcome carries what the caller cannot work out for itself: the
/// ts Slack minted, the channel's own name (the input names an id), and
/// how many messages the text was split into.
#[tokio::test]
async fn an_approved_draft_posts_exactly_once() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", "hello")).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    let outcome = task.await.expect("no panic").expect("an approved draft posts");
    assert_eq!(api.posts().len(), 1, "exactly one message goes out");
    assert_eq!(outcome.ts, vec!["1.0".to_owned()], "the ts the wire minted");
    assert_eq!(
        outcome.conversation_name, "ops",
        "the agent-facing name is the bare one every other tool reports",
    );
    assert_eq!(outcome.parts, 1, "a short draft is one part");
}

/// A draft past the split point goes out in several messages, and the
/// outcome's own count is the only thing that says so: a reader told
/// "one post" would take a truncated send for the whole text.
#[tokio::test]
async fn a_split_draft_reports_its_part_count() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let long = "x".repeat(forge_tools::slack::POST_LIMIT + 100);
    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.post(&caller(), post_request("C1", &long)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    let outcome = task.await.expect("no panic").expect("an approved draft posts");
    assert_eq!(outcome.parts, 2, "the split count travels with the ts");
    assert_eq!(outcome.ts.len(), 2, "and one ts per message really went out");
    assert_eq!(api.posts().len(), 2, "both parts reached Slack");
}

#[tokio::test]
async fn a_thread_reply_carries_the_parent_ts() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    let task = tokio::spawn({
        let facade = facade.clone();
        async move {
            let mut request = post_request("C1", "hi");
            request.thread_ts = Some("100.0".to_owned());
            facade.post(&caller(), request).await
        }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("posted");

    assert_eq!(api.posts()[0].2.as_deref(), Some("100.0"));
}

#[tokio::test]
async fn a_foreign_message_is_refused_by_the_wire_and_surfaced_as_its_reason() {
    // Slack's rule: only the authenticated user's messages can be
    // changed. The refusal is the wire's verdict - a local lookup
    // would wrongly refuse an edit of the user's own thread reply,
    // which history cannot see.
    let (facade, ws, api, _rx) = facade_with_recording_slack();
    api.refuse_edits_of("100.0");

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));

    assert_eq!(
        task.await.expect("no panic"),
        Err(SlackEditError::NotOwnMessage),
        "the wire's cant_update_message reads as not-your-message",
    );
    assert!(api.updates.lock().is_empty(), "a refused update changes nothing");
}

#[tokio::test]
async fn editing_own_message_updates_it_once_approved() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("an approved replacement is sent");

    assert_eq!(api.updates.lock().len(), 1);
}

#[tokio::test]
async fn a_rejected_edit_leaves_the_message_untouched() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));

    assert_eq!(task.await.expect("no panic"), Err(SlackEditError::Rejected));
    assert!(api.updates.lock().is_empty(), "a rejected replacement is never sent");
}

#[tokio::test]
async fn deleting_own_message_removes_it_once_approved() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", None)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(true));
    task.await.expect("no panic").expect("an approved deletion is sent");

    assert_eq!(api.deletes.lock().len(), 1);
}

#[tokio::test]
async fn a_rejected_delete_leaves_the_message_alone() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move { facade.edit(&caller(), edit_request("C1", "100.0", None)).await }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));

    assert_eq!(task.await.expect("no panic"), Err(SlackEditError::Rejected));
    assert!(api.deletes.lock().is_empty(), "a rejected deletion is never sent");
}

/// A reaction is authored content in the user's name, so both calls go
/// through the same approval a post does.
#[tokio::test]
async fn reacting_adds_then_removes() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move {
            facade.react(&caller(), react_request("C1", "100.0", "white_check_mark", true)).await?;
            facade.react(&caller(), react_request("C1", "100.0", "white_check_mark", false)).await
        }
    });
    let first = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(first, &caller(), answered(true));
    let second = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(second, &caller(), answered(true));

    task.await.expect("no panic").expect("both reactions applied");

    let reactions = api.reactions.lock().clone();
    assert_eq!(reactions.len(), 2);
    assert!(reactions[0].3, "the first call adds");
    assert!(!reactions[1].3, "the second removes");
}

#[tokio::test]
async fn a_rejected_reaction_is_never_sent() {
    let (facade, ws, api, _rx) = facade_with_recording_slack();

    let task = tokio::spawn({
        let facade = facade.clone();
        async move {
            facade.react(&caller(), react_request("C1", "100.0", "white_check_mark", true)).await
        }
    });
    let id = wait_for_draft(&ws).await;
    ws.slack.resolve_slack_draft(id, &caller(), answered(false));

    assert_eq!(task.await.expect("no panic"), Err(SlackReactError::Rejected));
    assert!(api.reactions.lock().is_empty(), "a rejected reaction is never applied");
}

/// Adding and removing a reaction are different acts, so the held draft
/// names which one it is: the card words its verbs from this field, and
/// a removal drawn as "React" asks the reader to approve the wrong act.
#[tokio::test]
async fn a_reaction_draft_names_whether_it_adds_or_removes() {
    let (facade, ws, _api, mut rx) = facade_with_recording_slack();

    for (add, expected) in
        [(false, SlackDraftOperation::Unreact), (true, SlackDraftOperation::React)]
    {
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.react(&caller(), react_request("C1", "100.0", "thumbsup", add)).await }
        });
        let mut seen = None;
        for _ in 0..200 {
            while let Ok(frame) = rx.try_recv() {
                if let crate::protocol::SessionUpdate::SlackPostPending { draft, .. } =
                    std::sync::Arc::unwrap_or_clone(frame.update)
                {
                    seen = Some(draft.operation);
                }
            }
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(seen, Some(expected), "the draft names the act it would do");
        let id = wait_for_draft(&ws).await;
        ws.slack.resolve_slack_draft(id, &caller(), answered(false));
        let _ = task.await;
    }
}

fn cfg(label: &str) -> SlackConfig {
    SlackConfig {
        workspace: label.to_owned(),
        token: "xoxp-test".to_owned(),
        poll_seconds: 30,
        dm_poll_seconds: 150,
        thread_idle_days: 14,
        stale_after_hours: 24,
    }
}

/// A workspace whose `[[slack]]` holds exactly `label`, with a lead
/// session recorded so the caller resolves to a project.
fn workspace_with_one_slack_workspace(label: &str) -> (Arc<Workspace>, Arc<dyn SlackFacade>) {
    let dir = tempdir().expect("tempdir");
    let mut config = LoadedConfig::empty_for_test();
    config.slack = vec![cfg(label)];
    let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
        .expect("the stub config's [[slack]] entries are well-formed");
    ws.seed_test_project_with_slack_workspace("forge", "/tmp/slack-facade-scope", label);
    ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
    let facade = prod_facade(&ws);
    (ws, facade)
}

/// The same, with a store installed: the cursor tests need real
/// watermarks. The tempdir is returned so it outlives the db.
fn workspace_with_one_slack_workspace_and_store(
    label: &str,
) -> (tempfile::TempDir, Arc<Workspace>, Arc<dyn SlackFacade>) {
    let dir = tempdir().expect("tempdir");
    let mut config = LoadedConfig::empty_for_test();
    config.slack = vec![cfg(label)];
    let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
        .expect("the stub config's [[slack]] entries are well-formed");
    ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"));
    ws.seed_test_project_with_slack_workspace("forge", "/tmp/slack-facade-scope", label);
    ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
    let facade = prod_facade(&ws);
    (dir, ws, facade)
}

/// `tokio::test`: a successful subscribe starts the workspace's pump,
/// which needs a runtime.
/// Deduplication is deliberately not done: two sessions may each want
/// their own mention feed, and merging them would silently starve one.
#[tokio::test]
async fn subscribing_twice_to_mentions_yields_two_records() {
    let (facade, ws, _api, _rx) = facade_with_recording_slack();

    facade.subscribe(&caller(), SlackSubscribeRequest::Mentions).expect("first");
    facade.subscribe(&caller(), SlackSubscribeRequest::Mentions).expect("second");

    assert_eq!(ws.slack.slack_subscriptions_for_project("forge").len(), 2);
}

/// A conversation is deduped where a mention feed is not: a repeat hands
/// back the record that exists rather than adding a second Inspector row
/// for one channel, and a mode the caller asks for is the mode it gets,
/// since there is no other route to change one.
#[tokio::test]
async fn subscribing_twice_to_a_conversation_yields_one_record() {
    let (facade, ws, _api, _rx) = facade_with_recording_slack();
    let watching = |mode| {
        SlackSubscribeRequest::Conversations(vec![SlackChannelWatch { id: "C1".to_owned(), mode }])
    };

    let first = facade.subscribe(&caller(), watching(SlackWatchMode::All)).expect("first");
    assert_eq!(first.len(), 1, "the first subscribe creates the record");

    let second =
        facade.subscribe(&caller(), watching(SlackWatchMode::MentionsOnly)).expect("second");
    assert_eq!(second.len(), 1, "the repeat adds no second record");
    assert_eq!(
        second[0].id, first[0].id,
        "the repeat hands back the id it already has, so the caller can drop it without a lookup",
    );
    assert_eq!(
        second[0].target,
        SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            name: None,
            mode: SlackWatchMode::MentionsOnly,
        },
        "and the record as it NOW stands, not the state before the mode moved",
    );
    let stored = ws.slack.slack_subscriptions_for_project("forge");
    assert_eq!(stored.len(), 1, "one record, so the Inspector renders one row");
    assert_eq!(
        stored[0].target,
        SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            name: None,
            mode: SlackWatchMode::MentionsOnly,
        },
        "and the mode asked for is the mode stored",
    );
}

/// The unsubscribe hands back the row it removed, not another one: with
/// two owned subscriptions and the SECOND removed, a row looked up by
/// "the project's first" names the wrong target - and the store check
/// catches a removal that took the wrong one.
#[tokio::test]
async fn unsubscribe_returns_the_row_it_removed_and_leaves_the_other() {
    let (facade, ws, _api, _rx) = facade_with_recording_slack();
    let first = facade.subscribe(&caller(), SlackSubscribeRequest::Mentions).expect("first");
    let second =
        facade.subscribe(&caller(), SlackSubscribeRequest::DirectMessages).expect("second");
    assert_eq!(second.len(), 1, "one record per target");

    let removed = facade
        .unsubscribe(&caller(), second[0].id)
        .expect("the second subscription is the caller's own");

    assert_eq!(removed.id, second[0].id, "the echoed row is the one that went");
    assert_eq!(
        removed.target,
        SlackSubscriptionTarget::DirectMessages,
        "carrying the target it watched, not the other row's",
    );
    let left = ws.slack.slack_subscriptions_for_project("forge");
    assert_eq!(left.len(), 1, "only the untouched subscription remains");
    assert_eq!(left[0].id, first[0].id, "and it is the one the call did not name");
}

/// The refusal names the case: a row another session owns reads as
/// *not yours*, never as absent.
#[tokio::test]
async fn unsubscribe_refuses_another_owners_row() {
    let (facade, ws, _api, _rx) = facade_with_recording_slack();
    let worker = live_worker(&ws, "forge", "tester");
    let owned =
        facade.subscribe(&caller(), SlackSubscribeRequest::Mentions).expect("the lead subscribes");

    assert_eq!(
        facade.unsubscribe(&worker, owned[0].id),
        Err(SlackUnsubscribeError::NotOwnedByCaller),
        "the lead's subscription is not the worker's to remove, and the refusal says which case it is",
    );
    assert_eq!(
        ws.slack.slack_subscriptions_for_project("forge").len(),
        1,
        "a refused unsubscribe removes nothing",
    );
}

/// The other half of the refusal: an id the project never had is a
/// different answer from one that exists under another session.
#[test]
fn unsubscribe_refuses_an_id_the_project_never_had() {
    let (facade, _ws, _api, _rx) = facade_with_recording_slack();

    assert_eq!(
        facade.unsubscribe(&caller(), Uuid::from_u128(0xdead)),
        Err(SlackUnsubscribeError::NoSuchSubscription),
        "an id with no entry in the project is reported as absent",
    );
}

/// Both clauses of that message are about the caller's own project:
/// an id living in another project is not "in this project but not
/// subscribed by you", it is not in this project at all.
#[test]
fn unsubscribe_refuses_an_id_from_another_project_as_absent() {
    let (facade, ws, _api, _rx) = facade_with_recording_slack();
    let other = SlackSubscription {
        id: Uuid::new_v4(),
        workspace: "acme".to_owned(),
        project: "otherproj".to_owned(),
        team_role: None,
        target: SlackSubscriptionTarget::Mentions,
        created_at: SystemTime::UNIX_EPOCH,
    };
    let other_id = other.id;
    ws.slack.add_slack_subscription(other, false);

    assert_eq!(
        facade.unsubscribe(&caller(), other_id),
        Err(SlackUnsubscribeError::NoSuchSubscription),
        "an id that lives in another project is absent here, not a subscription of this project's",
    );
}

/// A live worker row with `label`, so a caller on that slot resolves
/// to the project and owns subscriptions under its label.
fn live_worker(ws: &Workspace, project: &str, label: &str) -> SessionSlot {
    let slot = SessionSlot::worker("TestOrg", project, label);
    let project_key = ws
        .list_projects()
        .into_iter()
        .find(|view| view.name == project)
        .expect("the seeded project")
        .key;
    ws.insert_live_worker(
        &project_key,
        crate::mcp::workers::types::WorkerEntry {
            label: label.into(),
            charter: "c".into(),
            slot: slot.clone(),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", project),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            cwd: None,
            diagnostic: None,
            kick: None,
        },
    );
    slot
}

/// The refusal is what a caller acts on, so it has to name the case:
/// a row the project holds under another session reads as *not
/// yours*, an id the project never had as *not there*. One string
/// for both lets a caller take its own refused unsubscribe as proof
/// the subscription is gone.
#[tokio::test]
async fn unsubscribe_refusals_name_the_case_through_the_tool() {
    let (ws, facade) = workspace_with_one_slack_workspace("acme");
    let worker = live_worker(&ws, "forge", "tester");
    let lead_sub =
        facade.subscribe(&caller(), SlackSubscribeRequest::Mentions).expect("the lead subscribes");
    let tool = forge_tools::slack::SlackTools::new(facade, worker);

    let not_yours = forge_mcp::ToolProvider::call(
        &tool,
        "slack__unsubscribe",
        serde_json::json!({ "id": lead_sub[0].id.to_string() }),
    )
    .await
    .expect("the slack family owns slack__unsubscribe")
    .expect("the tool answers");
    assert!(not_yours.is_error, "a refused unsubscribe is an error");
    assert_eq!(
        text_of(&not_yours),
        format!(
            "subscription {} exists in this project but was not subscribed by you",
            lead_sub[0].id
        ),
        "a live subscription owned by another session must say so, not report it missing",
    );

    let ghost = Uuid::from_u128(0xdead);
    let not_there = forge_mcp::ToolProvider::call(
        &tool,
        "slack__unsubscribe",
        serde_json::json!({ "id": ghost.to_string() }),
    )
    .await
    .expect("the slack family owns slack__unsubscribe")
    .expect("the tool answers");
    assert!(not_there.is_error, "an unknown id is an error");
    assert_eq!(
        text_of(&not_there),
        format!("no subscription with id {ghost} in this project"),
        "an id the project never had must not read as another session's subscription",
    );
}

/// A project with no binding is a configuration gap, not a missing
/// argument: the refusal names the key to add rather than asking for a
/// `workspace` the tool no longer takes.
#[tokio::test]
async fn a_caller_whose_project_binds_no_workspace_is_told_which_key_to_add() {
    let dir = tempdir().expect("tempdir");
    let mut config = LoadedConfig::empty_for_test();
    config.slack = vec![cfg("acme")];
    let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
        .expect("the stub config's [[slack]] entries are well-formed");
    ws.seed_test_project("forge", "/tmp/slack-facade-scope");
    ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
    let facade = prod_facade(&ws);

    let error = facade.pins(&caller(), "C1").await.expect_err("no binding to resolve");
    let SlackReadError::Workspace(error) = &error else {
        panic!("a missing binding is a workspace refusal: {error:?}");
    };
    assert_eq!(
        error,
        &SlackWorkspaceError::Unbound { org: "TestOrg".to_owned(), project: "forge".to_owned() },
        "the refusal names the scope that has no binding",
    );
}

/// A binding to a label added by a reload is set but has no client:
/// saying "add the key" would send the caller to a key that is there.
#[tokio::test]
async fn a_binding_to_a_workspace_added_since_boot_names_the_restart() {
    let dir = tempdir().expect("tempdir");
    let mut config = LoadedConfig::empty_for_test();
    config.slack = vec![cfg("acme")];
    let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
        .expect("the stub config's [[slack]] entries are well-formed");
    ws.seed_test_project_with_slack_workspace("forge", "/tmp/slack-facade-scope", "beta");
    ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
    let facade = prod_facade(&ws);

    let error = facade.pins(&caller(), "C1").await.expect_err("no client to read with");
    let SlackReadError::Workspace(error) = &error else {
        panic!("a missing client is a workspace refusal: {error:?}");
    };
    assert_eq!(
        error,
        &SlackWorkspaceError::BoundButNotLive { label: "beta".to_owned() },
        "the refusal says the workspace is bound but not loaded",
    );
    assert!(
        format_workspace_error(error).contains("needs a forge restart"),
        "and tells the caller what makes it load",
    );
}

#[tokio::test]
async fn subscribing_to_dms_then_a_channel_yields_two_records() {
    let (ws, facade) = workspace_with_one_slack_workspace("acme");
    let first = facade.subscribe(&caller(), SlackSubscribeRequest::DirectMessages).expect("dms");
    let second = facade
        .subscribe(
            &caller(),
            SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                id: "C1".to_owned(),
                mode: SlackWatchMode::All,
            }]),
        )
        .expect("channels");
    assert_eq!(first.len() + second.len(), 2);
    assert_eq!(ws.slack.slack_subscriptions_for_project("forge").len(), 2);
}

/// A re-subscribe after the cursor cleared seeds fresh: the sweep
/// baselines at the newest message seen instead of replaying the gap.
#[tokio::test]
async fn re_subscribing_after_a_gap_baselines_fresh() {
    let (_dir, ws, facade) = workspace_with_one_slack_workspace_and_store("acme");
    facade
        .subscribe(
            &caller(),
            SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                id: "C1".to_owned(),
                mode: SlackWatchMode::All,
            }]),
        )
        .expect("the first subscribe");
    ws.slack.set_slack_watermark("acme", "C1", "100.0");
    let id = ws.slack.slack_subscriptions_for_project("forge")[0].id;
    assert!(ws.slack.remove_slack_subscription_owned_by("forge", id, None).is_some());

    facade
        .subscribe(
            &caller(),
            SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                id: "C1".to_owned(),
                mode: SlackWatchMode::All,
            }]),
        )
        .expect("re-subscribe");

    let cursor = {
        let db = ws.db.lock();
        crate::store::slack::watermark(db.as_ref().expect("db installed"), "acme", "C1")
            .expect("read")
    };
    assert_ne!(
        cursor,
        Some("100.0".to_owned()),
        "a fresh subscription starts from now, never from the stale cursor",
    );
    assert!(cursor.is_some(), "and it is seeded, so the sweep does not replay");
}

/// The cursor belongs to the conversation and is shared by every
/// owner watching it: a second subscriber seeds only when none
/// exists, or every owner's pending window is silently dropped.
#[tokio::test]
async fn subscribing_to_a_watched_conversation_does_not_reset_its_cursor() {
    let (_dir, ws, facade) = workspace_with_one_slack_workspace_and_store("acme");
    ws.slack.set_slack_watermark("acme", "C1", "100.0");

    facade
        .subscribe(
            &caller(),
            SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                id: "C1".to_owned(),
                mode: SlackWatchMode::All,
            }]),
        )
        .expect("subscribe to the watched conversation");

    let db = ws.db.lock();
    let cursor = crate::store::slack::watermark(db.as_ref().expect("db installed"), "acme", "C1")
        .expect("read");
    assert_eq!(cursor, Some("100.0".to_owned()), "a second subscriber never moves the cursor");
}

/// The mention cursor is one per workspace: a second Mentions
/// subscription seeds nothing. A fresh workspace still seeds, because
/// the mention sweep has no baseline arm and a missing cursor would
/// replay its whole history.
#[tokio::test]
async fn a_second_mention_subscription_seeds_nothing_but_the_first_seeds() {
    let (_dir, ws, facade) = workspace_with_one_slack_workspace_and_store("acme");

    facade
        .subscribe(&caller(), SlackSubscribeRequest::Mentions)
        .expect("the first mention subscriber");
    let first = {
        let db = ws.db.lock();
        crate::store::slack::watermark(db.as_ref().expect("db installed"), "acme", MENTION_CURSOR)
            .expect("read")
    };
    assert!(first.is_some(), "the first Mentions subscription seeds the stream");

    ws.slack.set_slack_watermark("acme", MENTION_CURSOR, "100.0");
    facade
        .subscribe(&caller(), SlackSubscribeRequest::Mentions)
        .expect("the second Mentions subscription");
    let second = {
        let db = ws.db.lock();
        crate::store::slack::watermark(db.as_ref().expect("db installed"), "acme", MENTION_CURSOR)
            .expect("read")
    };
    assert_eq!(
        second,
        Some("100.0".to_owned()),
        "the second subscriber never resets the shared mention cursor",
    );
}

fn directory_user(
    id: &str,
    name: &str,
    real_name: Option<&str>,
    display_name: Option<&str>,
    deleted: bool,
) -> SlackUser {
    SlackUser {
        id: id.to_owned(),
        name: name.to_owned(),
        real_name: real_name.map(str::to_owned),
        tz: None,
        deleted,
        profile: display_name
            .map(|display_name| SlackUserProfile { display_name: Some(display_name.to_owned()) }),
    }
}

/// The three names a person is known by are all the same query: a
/// handle, a legal name and what Slack shows as the display name.
#[tokio::test]
async fn a_user_resolver_matches_the_handle_the_real_name_and_the_display_name() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_users(vec![
        directory_user("U1", "ved", Some("Vedhavyas S"), Some("ved"), false),
        directory_user("U2", "gina", Some("Gina Abrams"), None, false),
        directory_user("U3", "csmith", None, Some("Chris"), false),
    ]);

    let (_, by_handle) = facade.users(&caller(), "VED").await.expect("handle, case-insensitive");
    assert_eq!(by_handle.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(), ["U1"]);

    let (_, by_real_name) = facade.users(&caller(), "abrams").await.expect("real name");
    assert_eq!(by_real_name.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(), ["U2"]);

    let (_, by_display_name) = facade.users(&caller(), "chris").await.expect("display name");
    assert_eq!(
        by_display_name.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(),
        ["U3"],
        "the display name matches even when neither the handle nor the real name does",
    );
}

/// An empty query must not widen into the whole directory when the
/// facade is called directly rather than through the tool.
#[tokio::test]
async fn a_user_resolver_returns_nothing_for_an_empty_query() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_users(vec![directory_user("U1", "ved", None, None, false)]);

    let (_, found) = facade.users(&caller(), "   ").await.expect("the guard is not a failure");

    assert!(found.is_empty(), "an empty query matches nobody");
}

/// The failure this resolver exists for is a wrong id silently
/// notifying the wrong person, so several matches come back together
/// and a deactivated member is never one of them.
#[tokio::test]
async fn a_user_resolver_returns_every_live_match_and_no_deleted_ones() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_users(vec![
        directory_user("U2", "samuel", None, None, false),
        directory_user("U1", "sam", None, None, false),
        directory_user("U3", "sam", None, None, true),
        directory_user("U4", "other", None, None, false),
    ]);

    let (_, found) = facade.users(&caller(), "sam").await.expect("the directory is read");

    assert_eq!(
        found.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(),
        ["U1", "U2"],
        "every live match, sorted by handle, never one picked silently",
    );
}

/// A filtered search follows the cursor: the first page may hold no hit
/// from the conversation, and stopping there would report none.
#[tokio::test]
async fn a_filtered_search_pages_until_it_finds_the_conversations_hits() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_search_pages(vec![vec![search_hit("C2", "100.1")], vec![search_hit("C1", "100.2")]]);

    let (_, found) =
        facade.search(&caller(), "deploy", 20, Some("C1")).await.expect("the search runs");

    assert_eq!(found.len(), 1, "the match was on the second page: {found:?}");
    assert_eq!(found[0].conversation_id, "C1");
    assert_eq!(
        api.search_reads(),
        ["in:ops deploy", "in:ops deploy"],
        "the filter followed the cursor instead of stopping at the first page",
    );
}

/// The label is part of the answer: `auth.test` reports the team, and a
/// `[[slack]]` label may well be a different string.
#[tokio::test]
async fn an_identity_lookup_names_the_resolved_label_and_the_token_user() {
    let (facade, _ws, _api, _rx) = facade_with_recording_slack();

    let (label, auth) = facade.me(&caller()).await.expect("the token is proved");

    assert_eq!(label, "acme", "the label the caller can name again");
    assert_eq!(auth.team, "Test", "the team comes from auth.test");
    assert_eq!(auth.user_id, "U1", "and so does the token's own user id");
}

#[tokio::test]
async fn a_pins_read_reaches_the_workspace_client_with_its_conversation() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_pins(
        "C1",
        vec![SlackPin {
            created: 1_700_000_000,
            created_by: Some("U1".to_owned()),
            message: Some(SlackPinMessage {
                ts: "1700000000.000100".to_owned(),
                user: Some("U2".to_owned()),
                text: "the pinned text".to_owned(),
            }),
        }],
    );

    let (_, pins) = facade.pins(&caller(), "C1").await.expect("read");
    assert_eq!(pins.len(), 1, "the seeded row comes back");
    assert_eq!(pins[0].message.as_ref().expect("the message rides along").text, "the pinned text",);
    assert_eq!(api.pin_reads.lock().as_slice(), ["C1"], "the conversation is what was read");
}

fn history_message(ts: &str, text: &str) -> SlackHistoryMessage {
    SlackHistoryMessage {
        ts: ts.to_owned(),
        user: Some("U1".to_owned()),
        text: text.to_owned(),
        thread_ts: None,
        reply_count: 0,
        bot_name: None,
        parent_user_id: None,
        latest_reply: None,
        edited: None,
        files: Vec::new(),
    }
}

fn search_hit(conversation: &str, ts: &str) -> SlackSearchMatch {
    SlackSearchMatch {
        ts: ts.to_owned(),
        text: "a hit".to_owned(),
        conversation_id: conversation.to_owned(),
        conversation_name: None,
        username: None,
        user: None,
        thread_ts: None,
        files: Vec::new(),
    }
}

#[tokio::test]
async fn a_history_read_passes_its_bounds_and_its_page() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_history("C1", vec![history_message("100.1", "hello")]);

    let (_, messages) =
        facade.history(&caller(), "C1", 25, Some("100.0"), Some("200.0")).await.expect("read");

    assert_eq!(messages.len(), 1, "the seeded page comes back");
    assert_eq!(messages[0].text, "hello");
    assert_eq!(
        api.history_reads().as_slice(),
        [("C1".to_owned(), Some("100.0".to_owned()), Some("200.0".to_owned()), 25)],
        "the conversation and both exclusive bounds reach the wire",
    );
}

#[tokio::test]
async fn a_thread_read_returns_the_parent_and_its_replies() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_replies(
        "C1",
        "100.0",
        vec![history_message("100.0", "parent"), history_message("100.5", "reply")],
    );

    let (_, messages) =
        facade.replies(&caller(), "C1", "100.0", 200).await.expect("the thread is read");

    assert_eq!(messages.len(), 2, "the whole thread comes back");
    assert_eq!(messages[0].text, "parent", "the parent is first");
    assert_eq!(messages[1].text, "reply");
    assert_eq!(
        api.reply_reads().as_slice(),
        [("C1".to_owned(), "100.0".to_owned())],
        "the conversation and parent ts are what was read",
    );
}

/// Slack's search language narrows by channel NAME, and an id in `in:`
/// finds nothing. The name is added server-side, the id still filters
/// what comes back, and a hit from another conversation never leaks.
#[tokio::test]
async fn a_search_filtered_by_a_named_conversation_narrows_and_drops_foreign_hits() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_conversation("C1", "ops");
    api.seed_search(vec![search_hit("C1", "100.1"), search_hit("C2", "100.2")]);

    let (_, found) =
        facade.search(&caller(), "deploy", 20, Some("C1")).await.expect("the search runs");

    assert_eq!(found.len(), 1, "only the filtered conversation's hit survives: {found:?}");
    assert_eq!(found[0].conversation_id, "C1");
    assert_eq!(api.search_reads(), ["in:ops deploy"], "the name narrows the search server-side");
}

/// A DM has no name, and a group DM's generated name is not one search
/// understands, so neither narrows the query: the id alone filters.
#[tokio::test]
async fn a_search_filtered_by_a_dm_or_group_dm_keeps_the_query_whole() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_dm("D1", "U9");
    api.seed_mpim("G1", "mpdm-ved--gina-1");
    api.seed_search(vec![
        search_hit("D1", "100.1"),
        search_hit("G1", "100.2"),
        search_hit("C2", "100.3"),
    ]);

    let (_, direct) =
        facade.search(&caller(), "deploy", 20, Some("D1")).await.expect("the search runs");
    assert_eq!(direct.len(), 1, "only the DM's hit survives: {direct:?}");
    assert_eq!(direct[0].conversation_id, "D1");

    let (_, group) =
        facade.search(&caller(), "deploy", 20, Some("G1")).await.expect("the search runs");
    assert_eq!(group.len(), 1, "only the group DM's hit survives: {group:?}");
    assert_eq!(group[0].conversation_id, "G1");

    assert_eq!(
        api.search_reads(),
        ["deploy", "deploy"],
        "neither a DM nor a group DM adds an `in:`",
    );
}

#[tokio::test]
async fn a_bookmarks_read_reaches_the_workspace_client_with_its_conversation() {
    let (facade, _ws, api, _rx) = facade_with_recording_slack();
    api.seed_bookmarks(
        "C1",
        vec![SlackBookmark {
            id: "Bk1".to_owned(),
            title: Some("Runbook".to_owned()),
            link: Some("https://example.com".to_owned()),
        }],
    );

    let (_, bookmarks) = facade.bookmarks(&caller(), "C1").await.expect("read");
    assert_eq!(bookmarks.len(), 1, "the seeded row comes back");
    assert_eq!(bookmarks[0].title.as_deref(), Some("Runbook"));
    assert_eq!(api.bookmark_reads.lock().as_slice(), ["C1"]);
}

/// The list marks only the CALLER's own subscriptions: a lead and a
/// worker in the same project each see their own records, never each
/// other's.
#[tokio::test]
async fn subscribed_targets_scopes_to_the_callers_role() {
    let (ws, facade) = workspace_with_one_slack_workspace("acme");
    let lead = caller();
    facade
        .subscribe(
            &lead,
            SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                id: "C1".to_owned(),
                mode: SlackWatchMode::All,
            }]),
        )
        .expect("the lead subscribes to C1");

    let worker_key = SessionSlot::worker("TestOrg", "forge", "tester");
    let project_key = ws
        .list_projects()
        .into_iter()
        .find(|view| view.name == "forge")
        .expect("the seeded project")
        .key;
    ws.insert_live_worker(
        &project_key,
        crate::mcp::workers::types::WorkerEntry {
            label: "tester".into(),
            charter: "c".into(),
            slot: worker_key.clone(),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", "forge"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            cwd: None,
            diagnostic: None,
            kick: None,
        },
    );
    facade
        .subscribe(&worker_key, SlackSubscribeRequest::DirectMessages)
        .expect("the worker subscribes to the DM class");

    let lead_targets: Vec<SlackSubscriptionTarget> =
        facade.subscribed_targets(&lead, "acme").into_iter().map(|s| s.target).collect();
    assert_eq!(
        lead_targets,
        vec![SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            name: None,
            mode: SlackWatchMode::All,
        }],
        "the lead sees only its own record",
    );
    let worker_targets: Vec<SlackSubscriptionTarget> =
        facade.subscribed_targets(&worker_key, "acme").into_iter().map(|s| s.target).collect();
    assert_eq!(
        worker_targets,
        vec![SlackSubscriptionTarget::DirectMessages],
        "the worker sees only its own record, never the lead's",
    );
}
