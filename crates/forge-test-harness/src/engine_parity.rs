//! Differential engine parity matrix for the certified `claude-headless`
//! port.
//!
//! A drop-in engine is judged against the reference on every answer forge
//! consumes, not on one smoke test. Each check spawns the same session
//! shape and environment on `claude` and on `claude-headless`, projects the
//! captured answer into a JSON value, and diffs the two values field by
//! field. A difference panics naming the field and both values:
//! `rawMaxTokens: reference=1048576 port=200000`.
//!
//! ## What a check may compare
//!
//! **A field the port answers differently fails by default.** The allowlist
//! below is the complete set of values that are allowed to differ, and each
//! entry says why. Values that merely happen to be unstable (model prose,
//! sampling) are not compared at all: a check projects the frames it owns
//! into stable fields rather than dumping raw lines, so an unlisted key that
//! starts differing is a failure. The prompt and context token counters two
//! reference spawns legitimately disagree on are compared with a narrow
//! tolerance measured by the control run; output-token counters only as "both
//! sides reported a count", because thinking moves them between identical
//! runs; context percentages are compared within one point; every other value
//! must match exactly.
//!
//! **The matrix runs under the operator's Claude config on purpose.** It
//! inherits `CLAUDE_CONFIG_DIR` (or `~/.claude`) so the port is judged on the
//! command, skill, agent and hook surface the operator actually has: a port
//! that enumerates none of it must fail even though it would match on a bare
//! config dir. The same run therefore greens differently on another machine,
//! and `just engine-parity-control` is how operator noise is told apart from
//! engine behaviour: it runs every check twice on the reference, and a field
//! that differs there is noise that must not be read as a port finding.
//! Transcripts the probe writes are removed after each session.
//!
//! Runs are opted into with `#[ignore]` and the `just engine-parity` recipe.
//! They are never part of `just check` or CI: both binaries and a real model
//! call are required.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use forge_primitives::{PermissionDecision, ToolPermissionContext};
use forge_sdk::{
    Client, ClientEvents, HookContext, HookDecision, HooksBuilder, OptionsBuilder, PermissionMode,
    PostToolUseInput, PreToolUseInput,
};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

/// The reference engine.
pub const REFERENCE_BINARY: &str = "claude";
/// The certified drop-in port under test.
pub const PORT_BINARY: &str = "claude-headless";

/// The prompt every model round uses where the answer is irrelevant: one
/// deterministic word, so a turn costs almost nothing.
const TRIVIAL_PROMPT: &str = "Reply with exactly the word OK.";

/// The account env this matrix runs the project under. forge's account
/// entry sets it to the 1M window; pinning it here means the window red is
/// reproducible even when the matrix is launched outside a forge session.
const MAX_CONTEXT_TOKENS: (&str, &str) = ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "1048576");

/// Values that differ per spawn for reasons that are not engine behaviour,
/// so a check may compare them only as placeholders. Anything not named here
/// and not projected away fails the check when it differs.
///
/// - `session_id`, `uuid`: minted per spawn.
/// - `request_id`: minted by forge-sdk per control request.
/// - `timestamp`, `duration_ms`, `durationMs`, `duration_api_ms`,
///   `ttft_ms`, `ttft_stream_ms`, `time_to_request_ms`,
///   `first_content_frame_ms`: wall clock.
/// - `cwd`, `transcript_path`, `memory_paths`, `messaging_socket_path`,
///   `user_output_styles_dir`: filesystem paths.
/// - `prompt_id`, `tool_use_id`: per-turn and per-call identifiers.
/// - `pid`: process identity.
/// - `account`: the signed-in account's identity fields.
/// - `session_state`, `tool_use_result`: per-session bookkeeping that carries
///   identifiers inside a nested value.
const ENGINE_SPECIFIC_KEYS: &[&str] = &[
    "session_id",
    "uuid",
    "request_id",
    "timestamp",
    "duration_ms",
    "durationMs",
    "duration_api_ms",
    "ttft_ms",
    "ttft_stream_ms",
    "time_to_request_ms",
    "first_content_frame_ms",
    "cwd",
    "transcript_path",
    "memory_paths",
    "messaging_socket_path",
    "user_output_styles_dir",
    "prompt_id",
    "tool_use_id",
    "pid",
    "account",
    "session_state",
    "tool_use_result",
];

/// True when the matrix is running as a reference-vs-reference control
/// (`FORGE_PARITY_CONTROL=1`, the `just engine-parity-control` recipe). In
/// control mode every check is captured twice on the reference and the two
/// captures are diffed; a field that differs there is operator noise, not an
/// engine finding.
pub fn control_mode() -> bool {
    std::env::var_os("FORGE_PARITY_CONTROL").is_some_and(|value| !value.is_empty())
}

/// Why the matrix cannot run here, or `None` when both engines are present.
pub fn unavailable_reason() -> Option<String> {
    for binary in [REFERENCE_BINARY, PORT_BINARY] {
        if !on_path(binary) {
            return Some(format!(
                "`{binary}` is not on PATH; the matrix needs both engines and a real model call"
            ));
        }
    }
    None
}

/// Whether `binary` names an executable file on `PATH`. Deliberately does
/// not spawn it: a missing port must skip, not fail.
fn on_path(binary: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(binary))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    })
}

/// Every frame either side of the wire produced, in arrival order. Filled by
/// the SDK's own tee callbacks, so a frame is recorded whether or not a typed
/// decoder models it.
#[derive(Clone, Default)]
pub struct Wire {
    inbound: Arc<Mutex<Vec<String>>>,
    outbound: Arc<Mutex<Vec<String>>>,
}

impl Wire {
    fn attach(builder: OptionsBuilder) -> (OptionsBuilder, Self) {
        let wire = Self::default();
        let inbound = Arc::clone(&wire.inbound);
        let outbound = Arc::clone(&wire.outbound);
        let builder = builder
            .tee_inbound(move |line: &str| inbound.lock().push(line.to_owned()))
            .tee_outbound(move |line: &str| outbound.lock().push(line.to_owned()));
        (builder, wire)
    }

    fn inbound_frames(&self) -> Vec<Value> {
        self.inbound.lock().iter().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    fn outbound_frames(&self) -> Vec<Value> {
        self.outbound.lock().iter().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    /// The `system/init` frame, which the CLI emits when the first user
    /// message starts the session.
    pub fn init_frame(&self) -> Option<Value> {
        self.inbound_frames().into_iter().find(|frame| {
            frame.get("type").and_then(Value::as_str) == Some("system")
                && frame.get("subtype").and_then(Value::as_str) == Some("init")
        })
    }

    fn control_request_ids(&self, subtype: &str) -> Vec<String> {
        self.outbound_frames()
            .iter()
            .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("control_request"))
            .filter(|frame| {
                frame.pointer("/request/subtype").and_then(Value::as_str) == Some(subtype)
            })
            .filter_map(|frame| frame.get("request_id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    /// The `control_response` envelopes answering requests of `subtype`.
    pub fn control_responses(&self, subtype: &str) -> Vec<Value> {
        let ids = self.control_request_ids(subtype);
        self.inbound_frames()
            .into_iter()
            .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("control_response"))
            .filter(|frame| {
                frame
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ids.iter().any(|known| known == id))
            })
            .collect()
    }

    /// The body of the last answer to a request of `subtype` - what the CLI
    /// put under `response.response`. `None` when the port never answered.
    pub fn control_body(&self, subtype: &str) -> Option<Value> {
        self.control_bodies(subtype).into_iter().next_back()
    }

    /// Every answer body to a request of `subtype`, in arrival order. A check
    /// that queries twice (before and after a turn) reads both here.
    pub fn control_bodies(&self, subtype: &str) -> Vec<Value> {
        self.control_responses(subtype)
            .iter()
            .filter_map(|frame| frame.pointer("/response/response").cloned())
            .collect()
    }

    /// The success/error discriminant of the last answer to `subtype`.
    pub fn control_answer(&self, subtype: &str) -> Option<Value> {
        self.control_responses(subtype)
            .last()
            .and_then(|frame| frame.pointer("/response/subtype").cloned())
    }

    /// Control requests the CLI sent us, by subtype: `hook_callback` and
    /// `can_use_tool` travel CLI -> SDK, the opposite direction from the
    /// requests forge initiates.
    pub fn inbound_control_requests(&self, subtype: &str) -> Vec<Value> {
        self.inbound_frames()
            .into_iter()
            .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("control_request"))
            .filter(|frame| {
                frame.pointer("/request/subtype").and_then(Value::as_str) == Some(subtype)
            })
            .collect()
    }

    /// The SDK's `control_response` frames answering the CLI's requests of
    /// `subtype` - the allow/deny decision forge writes back.
    pub fn answers_to_inbound(&self, subtype: &str) -> Vec<Value> {
        let ids: Vec<String> = self
            .inbound_control_requests(subtype)
            .iter()
            .filter_map(|frame| frame.get("request_id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        self.outbound_frames()
            .into_iter()
            .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("control_response"))
            .filter(|frame| {
                frame
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ids.iter().any(|known| known == id))
            })
            .collect()
    }

    /// The last `result` frame, the turn's closing answer.
    pub fn last_result(&self) -> Option<Value> {
        self.inbound_frames()
            .into_iter()
            .rfind(|frame| frame.get("type").and_then(Value::as_str) == Some("result"))
    }

    /// Turn frames in arrival order, without the traffic other checks own:
    /// control frames (check 6), the init frame (check 1), and
    /// `rate_limit_event`, whose presence depends on the account's current
    /// quota rather than on the engine.
    pub fn turn_frames(&self) -> Vec<Value> {
        self.inbound_frames()
            .into_iter()
            .filter(|frame| match frame.get("type").and_then(Value::as_str) {
                Some(
                    "control_response"
                    | "control_request"
                    | "control_cancel_request"
                    | "rate_limit_event",
                )
                | None => false,
                Some("system") => frame.get("subtype").and_then(Value::as_str) != Some("init"),
                Some(_) => true,
            })
            .collect()
    }
}

/// One difference between the reference and the port.
#[derive(Debug)]
pub struct Difference {
    /// Dotted path to the differing field, e.g. `usage.cache_read_input_tokens`.
    pub field: String,
    pub reference: Value,
    pub port: Value,
}

/// Compare a check's two captures and either print PASS or panic naming every
/// differing field with both values.
///
/// # Panics
///
/// On the first differing field's full list, so the failure output is the
/// deliverable rather than a bare assertion.
pub fn compare(check: &str, reference: &Value, port: &Value) {
    let differences = differences(reference, port);
    if differences.is_empty() {
        eprintln!("engine-parity {check}: PASS");
        return;
    }
    // Both full captures before the difference list: a reader judging the
    // red needs the matching fields too, not only the ones that moved.
    eprintln!("engine-parity {check}: reference={reference}");
    eprintln!("engine-parity {check}: port={port}");
    let mut report =
        format!("engine-parity {check}: FAIL ({} differing field(s))", differences.len());
    for difference in &differences {
        let _ = write!(
            report,
            "\n  {}: reference={} port={}",
            difference.field, difference.reference, difference.port
        );
    }
    panic!("{report}");
}

fn differences(reference: &Value, port: &Value) -> Vec<Difference> {
    let mut out = Vec::new();
    diff_at("", reference, port, &mut out);
    out
}

fn diff_at(path: &str, reference: &Value, port: &Value, out: &mut Vec<Difference>) {
    match (reference, port) {
        (Value::Object(reference_map), Value::Object(port_map)) => {
            let keys: BTreeSet<&String> = reference_map.keys().chain(port_map.keys()).collect();
            for key in keys {
                let field = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                match (reference_map.get(key), port_map.get(key)) {
                    (Some(reference_value), Some(port_value)) => {
                        diff_at(&field, reference_value, port_value, out);
                    }
                    (Some(reference_value), None) => out.push(Difference {
                        field,
                        reference: reference_value.clone(),
                        port: json!("<absent>"),
                    }),
                    (None, Some(port_value)) => out.push(Difference {
                        field,
                        reference: json!("<absent>"),
                        port: port_value.clone(),
                    }),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(reference_items), Value::Array(port_items)) => {
            if reference_items.len() != port_items.len() {
                out.push(Difference {
                    field: format!("{path}.length"),
                    reference: json!(reference_items.len()),
                    port: json!(port_items.len()),
                });
            }
            for (index, (reference_item, port_item)) in
                reference_items.iter().zip(port_items).enumerate()
            {
                diff_at(&format!("{path}[{index}]"), reference_item, port_item, out);
            }
        }
        (reference_value, port_value) if scalar_equal(reference_value, port_value) => {}
        (reference_value, port_value)
            if within_measured_noise(path, reference_value, port_value) => {}
        (reference_value, port_value) => out.push(Difference {
            field: path.to_owned(),
            reference: reference_value.clone(),
            port: port_value.clone(),
        }),
    }
}

/// Fields whose values two reference spawns legitimately differ on, measured
/// by `just engine-parity-control`, and what each is compared with.
///
/// Token counters are estimates: the same prompt and the same model answered
/// twice on `claude` differ by a few percent, because the conversation's own
/// round-trip and the turn's sampling are counted slightly differently each
/// time. The bands below come from the control's measured spread rather than a
/// guess, and the matrix's real findings still sit outside them: the accepted
/// port's context gap is 16% (69,568 against 58,397), against a measured
/// reference-vs-reference spread under 5%. A field absent from this list must
/// match exactly.
///
/// Output-token counts get no band. The control measured identical reference
/// runs at 106 and 3 output tokens: one turn spent 104 thinking tokens and the
/// other none, so the text itself differed by one token too. Thinking has no
/// budget under the trivial prompt, so a band wide enough for one control run
/// is a guess about the next one; what two engines can meaningfully agree on
/// is that both reported a count. Prompt tokens keep their measured band, and
/// the real finding still fails there (16% against the measured 4.8%).
fn within_measured_noise(field: &str, reference: &Value, port: &Value) -> bool {
    let (Some(reference), Some(port)) = (reference.as_f64(), port.as_f64()) else {
        return false;
    };
    match field {
        "usage.prompt_tokens" => {
            // Ten percent, and never less than 100 tokens, of the larger
            // value: `just engine-parity-control` measured identical reference
            // runs up to 4.8% apart (73,157 against 69,640) across seven
            // control runs on 2026-11-23, so this is about twice the worst
            // measured spread. The accepted port's context gap is 16% (69,568
            // against 58,397), which still fails.
            let scale = reference.abs().max(port.abs());
            (reference - port).abs() <= (scale * 0.10).max(100.0)
        }
        "before_turn.totalTokens" => {
            // One percent, and never less than 100 tokens, of the larger
            // value. The pre-turn snapshot is the bare context, which two
            // reference spawns reported identically on 2026-11-23.
            let scale = reference.abs().max(port.abs());
            (reference - port).abs() <= (scale * 0.01).max(100.0)
        }
        "after_turn.totalTokens" => {
            // Ten percent, and never less than 100 tokens, of the larger
            // value: the after-turn snapshot includes what the turn produced,
            // so thinking moves it as it moves the prompt and output counts.
            // `just engine-parity-control` measured identical reference runs
            // 4.5% apart (69,371 against 72,661).
            let scale = reference.abs().max(port.abs());
            (reference - port).abs() <= (scale * 0.10).max(100.0)
        }
        "before_turn.percentage" | "after_turn.percentage" => (reference - port).abs() <= 1.0,
        // Thinking is sampling and lands in both the result's aggregate and the
        // assistant frame's own count, so which count a turn reports is not an
        // engine fact. That a count was reported at all is.
        "usage.output_tokens" | "assistant_usage.output_tokens" => reference >= 0.0 && port >= 0.0,
        _ => false,
    }
}

/// Numbers compare by value so `1` and `1.0` are not a difference; every
/// other scalar must be exactly equal.
fn scalar_equal(reference: &Value, port: &Value) -> bool {
    match (reference, port) {
        (Value::Number(reference_number), Value::Number(port_number)) => {
            reference_number.as_f64() == port_number.as_f64()
        }
        (reference_value, port_value) => reference_value == port_value,
    }
}

/// Replace every allowlisted value with a placeholder, keeping the key so a
/// missing or extra field still fails.
fn mask(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut masked = Map::new();
            for (key, item) in map {
                if ENGINE_SPECIFIC_KEYS.contains(&key.as_str()) {
                    masked.insert(key.clone(), json!("<engine-specific>"));
                } else {
                    masked.insert(key.clone(), mask(item));
                }
            }
            Value::Object(masked)
        }
        Value::Array(items) => Value::Array(items.iter().map(mask).collect()),
        other => other.clone(),
    }
}

/// The type of every value in `shape`, recursively: keys and array lengths
/// are compared while values are not. Used where a check asks that two
/// answers have the same shape, not the same numbers.
fn shape(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut shaped = Map::new();
            for (key, item) in map {
                shaped.insert(key.clone(), shape(item));
            }
            Value::Object(shaped)
        }
        Value::Array(items) => Value::Array(items.iter().map(shape).collect()),
        Value::String(_) => json!("string"),
        Value::Number(_) => json!("number"),
        Value::Bool(_) => json!("boolean"),
        Value::Null => json!("null"),
    }
}

/// Sorted key names of an object, for frame-shape comparisons.
fn key_list(value: &Value) -> Value {
    key_list_without(value, &[])
}

/// [`key_list`] with keys the comparison deliberately ignores.
///
/// `user_message_uuid` / `user_message_uuids` are routing metadata the
/// reference CLI attaches to an assistant frame only when the turn continues
/// from a user message it queued, so their presence varies between turns of one
/// engine rather than between engines. An added routing key is additive and
/// forge's decode keeps it in `Extras`, so it is not a shape break.
fn key_list_without(value: &Value, ignored: &[&str]) -> Value {
    let Some(map) = value.as_object() else {
        return Value::Array(Vec::new());
    };
    let mut keys: Vec<&String> =
        map.keys().filter(|key| !ignored.contains(&key.as_str())).collect();
    keys.sort();
    Value::Array(keys.into_iter().map(|key| Value::String(key.clone())).collect())
}

const TURN_METADATA_KEYS: &[&str] = &["user_message_uuid", "user_message_uuids"];

/// Content-block shapes of a message envelope: the block type and its key
/// set. A bare string content (a plain user prompt) is one `string` block.
fn block_shapes(envelope: &Value) -> Value {
    match envelope.get("content") {
        Some(Value::String(_)) => json!([{ "type": "string", "keys": [] }]),
        Some(Value::Array(blocks)) => Value::Array(
            blocks
                .iter()
                .map(|block| {
                    json!({
                        "type": block.get("type").cloned().unwrap_or(Value::Null),
                        "keys": key_list(block),
                    })
                })
                .collect(),
        ),
        _ => Value::Array(Vec::new()),
    }
}

fn field(value: &Value, pointer: &str) -> Value {
    value.pointer(pointer).cloned().unwrap_or(Value::Null)
}

/// Run one session on `binary`, drive it, and return every frame it produced
/// with whatever the check projected.
///
/// Cleanup is bounded: stdin closes first, then the stream drains to EOF, and
/// trailing frames (a final `system/close` or `rate_limit_event`) are kept.
async fn session<F, Fut, T>(
    binary: &str,
    builder: OptionsBuilder,
    drive: F,
) -> Result<(Wire, T), String>
where
    F: FnOnce(Client, ClientEvents) -> Fut,
    Fut: Future<Output = Result<(Client, ClientEvents, T), String>>,
{
    let (builder, wire) = Wire::attach(builder);
    let options = builder.binary(binary).build();
    let (client, events) =
        Client::spawn(options).await.map_err(|error| format!("{binary}: spawn failed: {error}"))?;
    let (client, mut events, captured) = drive(client, events).await?;

    if let Err(error) = client.end_input().await {
        eprintln!("{binary}: end_input failed (continuing to drain): {error}");
    }
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) | Err(_) => break,
        }
    }
    if let Err(error) = client.disconnect().await {
        eprintln!("{binary}: disconnect failed (frames already captured): {error}");
    }
    remove_transcripts(&wire);
    Ok((wire, captured))
}

/// Remove the transcript files this session wrote, and nothing else.
///
/// A matrix run spawns roughly thirty short sessions, each of which the CLI
/// persists under `<config>/projects/<slug>/<session-id>.jsonl`. Only the ids
/// the captured frames name are touched, so a concurrent real session is safe;
/// a file that has not landed yet is left, and the next run's cleanup does not
/// revisit it. Best-effort by design: cleanup must never fail a check.
fn remove_transcripts(wire: &Wire) {
    let Some(config_dir) = config_dir() else {
        return;
    };
    let mut session_ids: Vec<String> = wire
        .inbound_frames()
        .iter()
        .filter_map(|frame| {
            frame
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .collect();
    session_ids.sort();
    session_ids.dedup();
    if session_ids.is_empty() {
        return;
    }
    let Ok(projects) = std::fs::read_dir(config_dir.join("projects")) else {
        return;
    };
    for project in projects.flatten() {
        let project_dir = project.path();
        if !project_dir.is_dir() {
            continue;
        }
        for session_id in &session_ids {
            let transcript = project_dir.join(format!("{session_id}.jsonl"));
            if transcript.is_file() {
                let _ = std::fs::remove_file(&transcript);
            }
            // Sub-agent transcripts live in a directory named for the session.
            let subagents = project_dir.join(session_id);
            if subagents.is_dir() {
                let _ = std::fs::remove_dir_all(&subagents);
            }
        }
    }
}

/// The config dir the spawned engines inherit: `CLAUDE_CONFIG_DIR`, else
/// `~/.claude`. Read here only to locate the transcripts a probe session wrote.
fn config_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(dir.into());
    }
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".claude"))
}

/// Wait for the turn's closing `result` frame. A transport error is a failed
/// capture: the check cannot judge a turn that never closed.
async fn await_result(events: &mut ClientEvents) -> Result<(), String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(120), events.recv()).await {
            Ok(Some(Ok(forge_primitives::Message::Result { .. }))) => return Ok(()),
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => return Err(format!("turn failed: {error}")),
            Ok(None) => return Err("stream closed before a result frame".to_owned()),
            Err(_) => return Err("no result frame within 120s".to_owned()),
        }
    }
}

/// Wait for the turn to stop for any reason, including an expected failure
/// (an unknown model, an interrupt). Captures are judged from the frames that
/// arrived, so a failed turn is not a failed check.
async fn drain_turn(events: &mut ClientEvents) {
    loop {
        match tokio::time::timeout(Duration::from_secs(60), events.recv()).await {
            Ok(Some(Ok(forge_primitives::Message::Result { .. }) | Err(_)) | None) | Err(_) => {
                return;
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// A control call bounded on our side: the SDK's `send_control` waits on the
/// port's answer with no budget of its own, so a port that never answers must
/// not wedge the matrix. A missing answer is recorded on the framed capture
/// and fails the check that owns the field.
async fn bounded<F, T>(what: &str, future: F) -> Option<T>
where
    F: Future<Output = Result<T, forge_sdk::Error>>,
{
    match tokio::time::timeout(Duration::from_secs(30), future).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(error)) => {
            eprintln!("{what}: no answer: {error}");
            None
        }
        Err(_) => {
            eprintln!("{what}: no answer within 30s");
            None
        }
    }
}

/// Shared spawn options: the account env's window, pinned so the probe can
/// reproduce the 1M-vs-200k red from any shell.
fn base_options() -> OptionsBuilder {
    OptionsBuilder::new().env(MAX_CONTEXT_TOKENS.0, MAX_CONTEXT_TOKENS.1)
}

/// (1) Init frame: model, tool names and count, MCP servers, slash commands,
/// agents, betas.
pub async fn capture_init_frame(binary: &str) -> Result<Value, String> {
    let (wire, ()) = session(binary, base_options().max_turns(1), |client, events| async move {
        client.send_user_message(TRIVIAL_PROMPT).await.map_err(|error| error.to_string())?;
        let mut events = events;
        await_result(&mut events).await?;
        Ok((client, events, ()))
    })
    .await?;
    let init = wire.init_frame().ok_or("no system/init frame was captured")?;
    Ok(json!({
        "model": field(&init, "/model"),
        "tools": sorted_strings(&field(&init, "/tools")),
        "tools_count": field(&init, "/tools").as_array().map_or(Value::Null, |tools| json!(tools.len())),
        "mcp_servers": mask(&sorted_by_name(field(&init, "/mcp_servers"))),
        "slash_commands": sorted_strings(&field(&init, "/slash_commands")),
        "agents": sorted_strings(&field(&init, "/agents")),
        "betas": mask(&field(&init, "/betas")),
    }))
}

/// (2) Context usage at spawn and after one trivial turn: the window and the
/// percentage derived from it. The pre-turn snapshot is the one a
/// just-started seat draws, so it is compared as its own set of fields.
pub async fn capture_context_usage(binary: &str) -> Result<Value, String> {
    let (wire, ()) = session(binary, base_options().max_turns(1), |client, events| async move {
        // Before any prompt: no init frame yet, but the usage query is local.
        let _ = bounded("get_context_usage (before turn)", client.get_context_usage()).await;
        client.send_user_message(TRIVIAL_PROMPT).await.map_err(|error| error.to_string())?;
        let mut events = events;
        await_result(&mut events).await?;
        let _ = bounded("get_context_usage (after turn)", client.get_context_usage()).await;
        Ok((client, events, ()))
    })
    .await?;
    let bodies = wire.control_bodies("get_context_usage");
    if bodies.len() < 2 {
        return Err(format!(
            "expected a get_context_usage answer before and after the turn, captured {}",
            bodies.len()
        ));
    }
    Ok(json!({
        "before_turn": context_snapshot(&bodies[0]),
        "after_turn": context_snapshot(&bodies[1]),
    }))
}

fn context_snapshot(body: &Value) -> Value {
    json!({
        "rawMaxTokens": field(body, "/rawMaxTokens"),
        "maxTokens": field(body, "/maxTokens"),
        "percentage": field(body, "/percentage"),
        "totalTokens": field(body, "/totalTokens"),
        "model": field(body, "/model"),
    })
}

/// (3) Usage and cost of one trivial turn.
///
/// Two identical turns run per side and the SECOND is compared: the first
/// warms that session's own prompt cache, so the compared turn starts from
/// the same cache state on both engines instead of depending on which binary
/// ran first. The turn's prompt-token total, its output-token counts and the
/// transcript's own usage entry are compared; the output counts only as
/// counts, since thinking is sampling and moves them (see the noise rules on
/// `within_measured_noise`). The cache split, the session cost derived from it
/// and the `modelUsage` aggregate are printed but excluded from the diff:
/// whether a token is an uncached input or a cache read depends on run order
/// against the shared account cache rather than on the engine.
pub async fn capture_usage_and_cost(binary: &str) -> Result<Value, String> {
    let (wire, ()) = session(binary, base_options().max_turns(4), |client, events| async move {
        let mut events = events;
        for _ in 0..2 {
            client.send_user_message(TRIVIAL_PROMPT).await.map_err(|error| error.to_string())?;
            await_result(&mut events).await?;
        }
        Ok((client, events, ()))
    })
    .await?;
    let result = wire.last_result().ok_or("no result frame was captured")?;
    let usage = field(&result, "/usage");
    // The compared turn's own transcript entry: the last assistant frame is
    // turn two's. Only its input and output counts are compared; the cache
    // split is not, for the same reason as the result frame's.
    let assistant_usage = wire
        .turn_frames()
        .into_iter()
        .rev()
        .find(|frame| frame.get("type").and_then(Value::as_str) == Some("assistant"))
        .map_or(Value::Null, |frame| {
            let frame_usage = field(&frame, "/message/usage");
            json!({
                "input_tokens": field(&frame_usage, "/input_tokens"),
                "output_tokens": field(&frame_usage, "/output_tokens"),
            })
        });
    // The cache split, the session cost and the modelUsage aggregate are
    // PRINTED but not compared. Whether a token is charged as an uncached
    // input or a cache read depends on which engine warmed the shared account
    // cache first, not on the engine; the same shift moves `costUSD`, which is
    // derived from the split. The prompt-token TOTAL is stable across that
    // shift and is what the system-prompt gap shows up in.
    let prompt_tokens: u64 =
        ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
            .iter()
            .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
            .sum();
    let assistant_frame_usage = wire
        .turn_frames()
        .into_iter()
        .rev()
        .find(|frame| frame.get("type").and_then(Value::as_str) == Some("assistant"))
        .map_or(Value::Null, |frame| field(&frame, "/message/usage"));
    eprintln!(
        "engine-parity usage_and_cost: {binary} observed turn_usage={usage} \
         assistant_usage={assistant_frame_usage} total_cost_usd={} modelUsage={}",
        field(&result, "/total_cost_usd"),
        field(&result, "/modelUsage")
    );
    Ok(json!({
        "usage": {
            "prompt_tokens": prompt_tokens,
            "output_tokens": field(&usage, "/output_tokens"),
        },
        "assistant_usage": assistant_usage,
        "model_facts": model_facts(&field(&result, "/modelUsage")),
    }))
}

/// Per-model facts a turn reports that do not move with cache state or token
/// counts.
fn model_facts(value: &Value) -> Value {
    let Some(models) = value.as_object() else {
        return Value::Null;
    };
    let mut facts = Map::new();
    for (model, usage) in models {
        facts.insert(
            model.clone(),
            json!({
                "contextWindow": field(usage, "/contextWindow"),
                "maxOutputTokens": field(usage, "/maxOutputTokens"),
            }),
        );
    }
    Value::Object(facts)
}

/// (4) Model resolution and the context-window env overrides.
///
/// Each case reads the model the engine resolved (`system/init`), the model
/// its context accounting uses, and the model echoed on the assistant frame -
/// the name that went on the wire. Cases that name a model the account does
/// not serve still resolve locally; a failed turn is tolerated because the
/// resolution is what is compared.
pub async fn capture_model_resolution(binary: &str) -> Result<Value, String> {
    let mut cases = Map::new();

    let default_case = model_case(binary, None, &[]).await?;
    let default_model = default_case.get("init_model").and_then(Value::as_str).map(str::to_owned);
    let Some(default_model) = default_model.filter(|model| !model.is_empty()) else {
        return Err("the default model case captured no resolved model".to_owned());
    };
    cases.insert("default".to_owned(), default_case);

    // The default can carry the suffix (a project configured for the 1M
    // window). Strip it so `plain` and `one_m_suffix` compare the two
    // spellings, and so the env overrides below run on the plain model: a
    // `[1m]` suffix would otherwise decide the window before the override is
    // read, hiding the env red behind the conditional `plain` case.
    let plain = default_model.strip_suffix("[1m]").unwrap_or(&default_model).to_owned();
    if plain != default_model {
        cases.insert("plain".to_owned(), model_case(binary, Some(plain.clone()), &[]).await?);
    }
    cases.insert(
        "one_m_suffix".to_owned(),
        model_case(binary, Some(format!("{plain}[1m]")), &[]).await?,
    );
    cases.insert("alias".to_owned(), model_case(binary, Some("sonnet".to_owned()), &[]).await?);
    cases.insert(
        "unknown".to_owned(),
        model_case(binary, Some("engine-parity-unrecognised-model".to_owned()), &[]).await?,
    );
    cases.insert(
        "max_context_override".to_owned(),
        model_case(binary, Some(plain.clone()), &[("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "300000")])
            .await?,
    );
    cases.insert(
        "auto_compact_override".to_owned(),
        model_case(binary, Some(plain), &[("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "250000")]).await?,
    );

    Ok(Value::Object(cases))
}

async fn model_case(
    binary: &str,
    model: Option<String>,
    env: &[(&str, &str)],
) -> Result<Value, String> {
    let mut builder = base_options().max_turns(1);
    if let Some(model) = model {
        builder = builder.model(model);
    }
    for (key, value) in env {
        builder = builder.env(*key, *value);
    }
    let (wire, ()) = session(binary, builder, |client, events| async move {
        client.send_user_message(TRIVIAL_PROMPT).await.map_err(|error| error.to_string())?;
        let mut events = events;
        drain_turn(&mut events).await;
        let _ = bounded("get_context_usage", client.get_context_usage()).await;
        Ok((client, events, ()))
    })
    .await?;

    let init_model = wire.init_frame().map_or(Value::Null, |init| field(&init, "/model"));
    let usage = wire.control_body("get_context_usage").unwrap_or(Value::Null);
    let wire_model = wire
        .turn_frames()
        .into_iter()
        .rev()
        .find(|frame| frame.get("type").and_then(Value::as_str) == Some("assistant"))
        .map_or(Value::Null, |frame| field(&frame, "/message/model"));
    Ok(json!({
        "init_model": init_model,
        "usage_model": field(&usage, "/model"),
        "wire_model": wire_model,
        "rawMaxTokens": field(&usage, "/rawMaxTokens"),
        "maxTokens": field(&usage, "/maxTokens"),
        "autoCompactThreshold": field(&usage, "/autoCompactThreshold"),
        "isAutoCompactEnabled": field(&usage, "/isAutoCompactEnabled"),
        "autocompactSource": field(&usage, "/autocompactSource"),
    }))
}

/// (5) Streaming frame shapes across a plain text turn and a tool call: the
/// frame types, their top-level key sets, and the content-block shapes.
/// Values (prose, ids, tool arguments) are not compared - the shape is.
pub async fn capture_streaming_shapes(binary: &str) -> Result<Value, String> {
    let builder = base_options()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(["Bash"]);
    let (wire, ()) = session(binary, builder, |client, events| async move {
        let mut events = events;
        client.send_user_message(TRIVIAL_PROMPT).await.map_err(|error| error.to_string())?;
        await_result(&mut events).await?;
        client
            .send_user_message(
                "Run `echo engine-parity` with the Bash tool, then reply with \
                 exactly what it printed.",
            )
            .await
            .map_err(|error| error.to_string())?;
        await_result(&mut events).await?;
        Ok((client, events, ()))
    })
    .await?;
    // Thinking frames are excluded: whether the model thinks at all on a given
    // run is sampling, so their presence cannot be an engine-parity fact.
    let shapes = distinct_frame_shapes(
        wire.turn_frames()
            .iter()
            .map(project_turn_frame)
            .filter(|frame| {
                !matches!(
                    frame_label(frame).as_str(),
                    "system/thinking_tokens" | "assistant:thinking"
                )
            })
            .collect(),
    );
    // The turns must have produced the frames this check exists to compare; an
    // empty or partial capture otherwise agrees with an empty or partial one.
    for required in ["assistant:text", "assistant:tool_use", "user:tool_result", "result"] {
        if shapes.get(required).is_none() {
            return Err(format!("capture incomplete: the turns produced no `{required}` frame"));
        }
    }
    Ok(shapes)
}

/// Key each frame's shape by its type, subtype and content-block types, so a
/// missing or extra kind of frame is ONE difference rather than a positional
/// cascade. Counts and interleaving are not compared: a heartbeat repeat rate
/// tracks the network, and the shapes are what the engine must agree on.
fn distinct_frame_shapes(frames: Vec<Value>) -> Value {
    let mut by_label = Map::new();
    for frame in frames {
        let label = frame_label(&frame);
        if let Some(existing) = by_label.get_mut(&label) {
            merge_frame_shape(existing, &frame);
        } else {
            by_label.insert(label, frame);
        }
    }
    Value::Object(by_label)
}

/// Fold one frame's shape into the label's accumulated shape: key lists become
/// their union, and a scalar the first frame lacked is filled in. A frame type
/// may carry optional routing keys on some instances and not others; the union
/// is the type's shape, while a key no instance ever carries still shows up as
/// absent.
fn merge_frame_shape(into: &mut Value, other: &Value) {
    let (Some(into_map), Some(other_map)) = (into.as_object_mut(), other.as_object()) else {
        return;
    };
    for (key, value) in other_map {
        match (into_map.get_mut(key), value) {
            (Some(Value::Array(existing)), Value::Array(incoming)) => {
                let mut union: Vec<String> = existing
                    .iter()
                    .chain(incoming)
                    .map(|item| item.as_str().map_or_else(|| item.to_string(), str::to_owned))
                    .collect();
                union.sort();
                union.dedup();
                *existing = union.into_iter().map(Value::String).collect();
            }
            (Some(existing), _) if existing.is_null() => *existing = value.clone(),
            _ => {}
        }
    }
}

fn frame_label(frame: &Value) -> String {
    let mut label = frame.get("type").and_then(Value::as_str).unwrap_or("<untyped>").to_owned();
    if let Some(subtype) = frame.get("subtype").and_then(Value::as_str) {
        label.push('/');
        label.push_str(subtype);
    }
    if let Some(blocks) = frame.get("content").and_then(Value::as_array) {
        let types: Vec<&str> =
            blocks.iter().filter_map(|block| block.get("type").and_then(Value::as_str)).collect();
        if !types.is_empty() {
            label.push(':');
            label.push_str(&types.join(","));
        }
    }
    label
}

fn project_turn_frame(frame: &Value) -> Value {
    let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let mut projected = Map::new();
    projected.insert("type".to_owned(), json!(frame_type));
    match frame_type {
        "system" => {
            projected.insert("subtype".to_owned(), field(frame, "/subtype"));
        }
        "assistant" => {
            let envelope = field(frame, "/message");
            projected.insert("keys".to_owned(), key_list_without(frame, TURN_METADATA_KEYS));
            projected.insert("message_keys".to_owned(), key_list(&envelope));
            projected.insert("content".to_owned(), block_shapes(&envelope));
            projected.insert("stop_reason".to_owned(), field(&envelope, "/stop_reason"));
            projected.insert("error".to_owned(), field(frame, "/error"));
        }
        "user" => {
            let envelope = field(frame, "/message");
            projected.insert("keys".to_owned(), key_list_without(frame, TURN_METADATA_KEYS));
            projected.insert("message_keys".to_owned(), key_list(&envelope));
            projected.insert("content".to_owned(), block_shapes(&envelope));
        }
        "result" => {
            projected.insert("keys".to_owned(), key_list(frame));
            projected.insert("stop_reason".to_owned(), field(frame, "/stop_reason"));
            projected.insert("terminal_reason".to_owned(), field(frame, "/terminal_reason"));
            projected.insert("is_error".to_owned(), field(frame, "/is_error"));
        }
        _ => {
            projected.insert("keys".to_owned(), key_list(frame));
        }
    }
    Value::Object(projected)
}

/// (6) Control protocol: every control request forge sends answers the same
/// subtype with the same shape. The initialize and short answers are compared
/// with engine-specific values masked; `get_context_usage` is compared as a
/// shape only, because its numbers are check 2's.
pub async fn capture_control_protocol(binary: &str) -> Result<Value, String> {
    let builder = base_options().max_turns(1);
    let (wire, ()) = session(binary, builder, |client, events| async move {
        let mut events = events;
        // A turn in flight is what `interrupt` is for; counting cannot finish
        // before the interrupt lands.
        client
            .send_user_message("Count from 1 to 500 slowly, one number per line.")
            .await
            .map_err(|error| error.to_string())?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let _ = bounded("interrupt", client.interrupt()).await;
        drain_turn(&mut events).await;

        let _ =
            bounded("set_permission_mode", client.set_permission_mode(PermissionMode::Plan)).await;
        let _ = bounded("mcp_status", client.mcp_status()).await;
        let _ = bounded("get_context_usage", client.get_context_usage()).await;
        Ok((client, events, ()))
    })
    .await?;

    // Every subtype must have been answered on this side, or the comparison
    // would pass two missing answers off as agreement.
    for subtype in
        ["initialize", "get_context_usage", "mcp_status", "interrupt", "set_permission_mode"]
    {
        if wire.control_answer(subtype).is_none() || wire.control_body(subtype).is_none() {
            return Err(format!("no answer to `{subtype}` was captured"));
        }
    }
    let mut protocol = Map::new();
    protocol.insert("initialize".to_owned(), answer(&wire, "initialize", false));
    protocol.insert("get_context_usage".to_owned(), answer(&wire, "get_context_usage", true));
    protocol.insert("mcp_status".to_owned(), answer(&wire, "mcp_status", false));
    protocol.insert("interrupt".to_owned(), answer(&wire, "interrupt", false));
    protocol.insert("set_permission_mode".to_owned(), answer(&wire, "set_permission_mode", false));
    Ok(Value::Object(protocol))
}

fn answer(wire: &Wire, subtype: &str, shape_only: bool) -> Value {
    let body = wire.control_body(subtype).unwrap_or(Value::Null);
    json!({
        "answer": wire.control_answer(subtype).unwrap_or(Value::Null),
        "body": if shape_only { shape(&body) } else { mask(&body) },
    })
}

/// (7) Hook payloads and the control response forge writes back. A pre and a
/// post hook on Bash run one tool call; the request inputs and the SDK's
/// answer are projected to their shapes plus the values that name the event.
pub async fn capture_hooks(binary: &str) -> Result<Value, String> {
    let hooks = HooksBuilder::new()
        .pre_tool_use("Bash", |_input: PreToolUseInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .post_tool_use("Bash", |_input: PostToolUseInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .build();
    let builder = base_options()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(["Bash"])
        .hooks(hooks);
    let (wire, ()) = session(binary, builder, |client, events| async move {
        let mut events = events;
        client
            .send_user_message(
                "Run `echo engine-parity-hook` with the Bash tool, then reply \
                 with exactly what it printed.",
            )
            .await
            .map_err(|error| error.to_string())?;
        await_result(&mut events).await?;
        Ok((client, events, ()))
    })
    .await?;

    let events: Vec<Value> = wire
        .inbound_control_requests("hook_callback")
        .iter()
        .map(|request| {
            let input = field(request, "/request/input");
            json!({
                "callback_id": field(request, "/request/callback_id"),
                "event": field(&input, "/hook_event_name"),
                "tool_name": field(&input, "/tool_name"),
                "request_keys": key_list(&field(request, "/request")),
                "input_keys": key_list(&input),
                "tool_input_keys": key_list(&field(&input, "/tool_input")),
                "tool_response_keys": key_list(&field(&input, "/tool_response")),
            })
        })
        .collect();
    let answers: Vec<Value> = wire
        .answers_to_inbound("hook_callback")
        .iter()
        .map(|response| {
            json!({
                "answer": field(response, "/response/subtype"),
                "body_keys": key_list(&field(response, "/response/response")),
            })
        })
        .collect();
    // A side that fired no hooks would compare two empty lists as agreement.
    for required in ["PreToolUse", "PostToolUse"] {
        let seen =
            events.iter().any(|event| event.get("event").and_then(Value::as_str) == Some(required));
        if !seen {
            return Err(format!(
                "capture incomplete: no {required} hook request was captured ({})",
                turn_outcome(&wire)
            ));
        }
    }
    if answers.is_empty() {
        return Err("capture incomplete: no hook_callback answer was captured".to_owned());
    }
    Ok(json!({
        "events": distinct_shapes(&events),
        "answers": distinct_shapes(&answers),
    }))
}

/// (8) Permission ask shape and the allow/deny outcome. A `Write` outside the
/// working directory asks, the callback denies, and the turn's own record of
/// the denial is compared too.
pub async fn capture_permission(binary: &str) -> Result<Value, String> {
    let builder = base_options()
        .max_turns(3)
        .permission_mode(PermissionMode::Ask)
        // The developer profile can auto-approve, so force the CLI's default
        // mode and route prompts over stream-json (the same recipe the
        // permission_deny wire scenario uses).
        .extra_arg("permission-mode", Some("default".to_owned()))
        .permission_prompt_tool_name("stdio")
        .can_use_tool(|_ctx: ToolPermissionContext| async move {
            PermissionDecision::deny("engine parity probe denies this tool")
        });
    let (wire, ()) = session(binary, builder, |client, events| async move {
        let mut events = events;
        client
            .send_user_message(
                "Use the Write tool to create /tmp/forge-engine-parity-deny.txt \
                 containing HELLO. Then say whether the Write succeeded.",
            )
            .await
            .map_err(|error| error.to_string())?;
        await_result(&mut events).await?;
        Ok((client, events, ()))
    })
    .await?;

    let asks: Vec<Value> = wire
        .inbound_control_requests("can_use_tool")
        .iter()
        .map(|request| {
            let input = field(request, "/request/input");
            json!({
                "tool_name": field(request, "/request/tool_name"),
                "display_name": field(request, "/request/display_name"),
                "decision_reason_type": field(request, "/request/decision_reason_type"),
                "request_keys": key_list(&field(request, "/request")),
                "input_keys": key_list(&input),
                "suggestion_types": suggestion_types(&field(request, "/request/permission_suggestions")),
            })
        })
        .collect();
    let answers: Vec<Value> = wire
        .answers_to_inbound("can_use_tool")
        .iter()
        .map(|response| {
            let body = field(response, "/response/response");
            json!({
                "answer": field(response, "/response/subtype"),
                "behavior": field(&body, "/behavior"),
                "message": field(&body, "/message"),
                "body_keys": key_list(&body),
            })
        })
        .collect();
    let denials = wire
        .last_result()
        .map(|result| {
            field(&result, "/permission_denials")
                .as_array()
                .map(|denials| {
                    denials
                        .iter()
                        .map(|denial| {
                            json!({
                                "tool_name": field(denial, "/tool_name"),
                                "keys": key_list(denial),
                            })
                        })
                        .collect::<Vec<Value>>()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    // A side that never asked would compare two empty lists as agreement, and
    // a turn that asked for some other tool would not exercise the write path.
    if asks.is_empty() {
        return Err(format!(
            "capture incomplete: no can_use_tool request was captured ({})",
            turn_outcome(&wire)
        ));
    }
    if !asks.iter().any(|ask| ask.get("tool_name") == Some(&json!("Write"))) {
        return Err(format!(
            "capture incomplete: the turn asked for no Write tool: asks={asks:?} ({})",
            turn_outcome(&wire)
        ));
    }
    if answers.is_empty() {
        return Err("capture incomplete: no can_use_tool answer was captured".to_owned());
    }
    if denials.is_empty() {
        return Err(
            "capture incomplete: the result frame recorded no permission_denials".to_owned()
        );
    }
    // Compare the FIRST ask and its outcome. How many times a model retries
    // after a denial, or which tool it falls back to, is sampling: the
    // permission surface is the ask shape and the decision forge writes back.
    Ok(json!({
        "ask": asks.first().cloned().unwrap_or(Value::Null),
        "answer": answers.first().cloned().unwrap_or(Value::Null),
        "denial": denials.first().cloned().unwrap_or(Value::Null),
    }))
}

/// What the turn did, for a capture failure that says why no ask arrived.
fn turn_outcome(wire: &Wire) -> String {
    let result = wire.last_result().unwrap_or(Value::Null);
    let blocks: Vec<String> = wire
        .turn_frames()
        .iter()
        .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("assistant"))
        .flat_map(|frame| {
            field(frame, "/message/content")
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| {
                            block.get("type").and_then(Value::as_str).map(str::to_owned)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    let tool_names: Vec<String> = wire
        .turn_frames()
        .iter()
        .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("assistant"))
        .flat_map(|frame| {
            field(frame, "/message/content")
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| {
                            block.get("name").and_then(Value::as_str).map(str::to_owned)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    let hooks = wire
        .outbound_frames()
        .into_iter()
        .find(|frame| {
            frame.pointer("/request/subtype").and_then(Value::as_str) == Some("initialize")
        })
        .map_or(Value::Null, |frame| field(&frame, "/request/hooks"));
    format!(
        "is_error={} result={} assistant_blocks={blocks:?} tools={tool_names:?} registered_hooks={hooks}",
        field(&result, "/is_error"),
        field(&result, "/result")
    )
}

/// The distinct shapes in `items`, serialised and sorted. A retried ask or a
/// repeated hook event is the same shape twice; how many times a model retries
/// after a denial is sampling, not engine behaviour, so repetitions collapse.
fn distinct_shapes(items: &[Value]) -> Value {
    let mut serialized: Vec<String> = items.iter().map(ToString::to_string).collect();
    serialized.sort();
    serialized.dedup();
    Value::Array(serialized.into_iter().map(Value::String).collect())
}

fn suggestion_types(suggestions: &Value) -> Value {
    let mut types: Vec<String> = suggestions
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("type").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    types.sort();
    Value::Array(types.into_iter().map(Value::String).collect())
}

fn sorted_strings(value: &Value) -> Value {
    let mut items: Vec<String> = value
        .as_array()
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    items.sort();
    Value::Array(items.into_iter().map(Value::String).collect())
}

/// Sort an array of objects by `name` so server order cannot fail a check
/// that is about membership. Objects without a name keep their wire order.
fn sorted_by_name(value: Value) -> Value {
    let Some(items) = value.as_array() else {
        return value;
    };
    let mut items = items.clone();
    items.sort_by(|left, right| {
        left.get("name").and_then(Value::as_str).cmp(&right.get("name").and_then(Value::as_str))
    });
    Value::Array(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `usage_and_cost` capture reduced to the fields the check compares,
    /// enough to drive `differences` the way the control run does.
    fn usage_capture(prompt_tokens: u64, output_tokens: u64, assistant_output: u64) -> Value {
        json!({
            "usage": {
                "prompt_tokens": prompt_tokens,
                "output_tokens": output_tokens,
            },
            "assistant_usage": {
                "input_tokens": 4,
                "output_tokens": assistant_output,
            },
            "model_facts": {
                "claude-opus-4-1[1m]": {
                    "contextWindow": 1_000_000,
                    "maxOutputTokens": 32_000,
                },
            },
        })
    }

    /// The `context_usage` capture reduced to the fields the check compares.
    fn context_capture(before_total: u64, after_total: u64) -> Value {
        json!({
            "before_turn": {
                "rawMaxTokens": 1_000_000,
                "maxTokens": 1_000_000,
                "percentage": 9,
                "totalTokens": before_total,
                "model": "deepseek-v4.1-flash[1m]",
            },
            "after_turn": {
                "rawMaxTokens": 1_000_000,
                "maxTokens": 1_000_000,
                "percentage": 7,
                "totalTokens": after_total,
                "model": "deepseek-v4.1-flash[1m]",
            },
        })
    }

    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default()
    }

    /// The shape `just engine-parity-control` failed on: identical reference
    /// runs whose output-token counts differ only by whether the turn thought.
    /// Both counters move together here, because thinking lands in whichever
    /// message carries it and the result aggregates it.
    #[test]
    fn control_shape_passes_when_only_output_tokens_move() {
        let first = usage_capture(69_609, 106, 106);
        let second = usage_capture(69_483, 3, 3);

        let differences = differences(&first, &second);
        assert!(differences.is_empty(), "{differences:?}");
    }

    /// The prompt-token band comes from the control's own measurement: its
    /// worst pair moved 4.8% between identical reference runs, and a band
    /// tighter than that is what failed the control.
    #[test]
    fn prompt_tokens_measured_control_spread_passes() {
        let first = usage_capture(73_157, 3, 0);
        let second = usage_capture(69_640, 3, 0);

        let differences = differences(&first, &second);
        assert!(differences.is_empty(), "{differences:?}");
    }

    /// The accepted port run's real finding must still fail, and the report
    /// must name the field and both values.
    #[test]
    fn prompt_token_context_gap_fails_naming_the_field_and_both_values() {
        let reference = usage_capture(69_568, 4, 4);
        let port = usage_capture(58_397, 4, 4);

        let panic =
            std::panic::catch_unwind(|| compare("control:usage_and_cost", &reference, &port))
                .expect_err("a prompt-token gap must fail");
        let message = panic_message(panic.as_ref());
        for expected in ["usage.prompt_tokens", "69568", "58397"] {
            assert!(message.contains(expected), "report is missing `{expected}`: {message}");
        }
    }

    /// The after-turn snapshot carries what the turn produced, so its measured
    /// control spread sets its band; the pre-turn snapshot is the bare context
    /// and stays tight. Both bands are pinned from above as well as below: a
    /// move past 10% fails on the after-turn total, and a 1,018-token pre-turn
    /// move fails even though a five-percent band would have accepted it.
    #[test]
    fn after_turn_measured_control_spread_passes_while_before_turn_stays_tight() {
        let first = context_capture(89_982, 69_371);
        let second = context_capture(89_982, 72_661);
        let after_differences = differences(&first, &second);
        assert!(after_differences.is_empty(), "{after_differences:?}");

        let within_band = context_capture(89_982, 69_371);
        let beyond_band = context_capture(89_982, 80_000);
        let after_differences = differences(&within_band, &beyond_band);
        assert_eq!(after_differences.len(), 1, "{after_differences:?}");
        assert_eq!(after_differences[0].field, "after_turn.totalTokens");

        for widened_total in [91_000, 95_000] {
            let narrowed = context_capture(89_982, 69_371);
            let widened = context_capture(widened_total, 69_371);
            let before_differences = differences(&narrowed, &widened);
            assert_eq!(before_differences.len(), 1, "{before_differences:?}");
            assert_eq!(before_differences[0].field, "before_turn.totalTokens");
        }
    }

    /// The percentage arm is a one-point band on its own: identical totals with
    /// percentages one point apart pass, two points apart fail.
    #[test]
    fn percentage_band_is_one_point() {
        let zero = context_capture(89_982, 69_371);

        let mut one_point = context_capture(89_982, 69_371);
        one_point["before_turn"]["percentage"] = json!(10);
        let within_band = differences(&zero, &one_point);
        assert!(within_band.is_empty(), "{within_band:?}");

        let mut two_points = context_capture(89_982, 69_371);
        two_points["before_turn"]["percentage"] = json!(11);
        let beyond_band = differences(&zero, &two_points);
        assert_eq!(beyond_band.len(), 1, "{beyond_band:?}");
        assert_eq!(beyond_band[0].field, "before_turn.percentage");
    }

    /// The output-token rule is not a free pass: a count that is absent or
    /// negative is still a difference.
    #[test]
    fn output_tokens_must_be_present_and_non_negative() {
        let reference = usage_capture(100, 10, 10);
        let mut negative = usage_capture(100, 10, 10);
        negative["usage"]["output_tokens"] = json!(-1);
        let mut absent = usage_capture(100, 10, 10);
        absent["usage"].as_object_mut().expect("usage object").remove("output_tokens");

        for capture in [&negative, &absent] {
            let differences = differences(&reference, capture);
            assert_eq!(differences.len(), 1, "{differences:?}");
            assert_eq!(differences[0].field, "usage.output_tokens");
        }
    }
}
