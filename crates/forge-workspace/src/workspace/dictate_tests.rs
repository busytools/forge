//! Dictation driven through the workspace's command bus and its update
//! stream: the integration half of the subsystem's tests. The policy and
//! config tests live in `forge-dictate` with the code they cover; these
//! need a real seat, the command bus and the wire, so they stay beside the
//! host that supplies them.
//!
//! The last test is the seam's own witness: the subsystem registers from
//! the workspace's config and a command reaches it and comes back as the
//! frame the client reads.

use super::*;

use crate::dictate::{
    DictateAxes, DictateOutcome, DictateOverrideUpdate, DictateOverrides, FinishingTake,
    LiveRecording, handle_dictate_stop,
};
use crate::{Command, SessionSlot, SessionUpdate};
use forge_dictate::{ConfigBuilder, Engine};
use std::time::Instant;

fn key(name: &str) -> SessionSlot {
    SessionSlot::from_str_for_test(name.to_owned())
}

/// An engine over an empty models directory: takes register, meter and
/// cap exactly as they do in production, and every job it runs answers
/// the load failure a weightless engine gives.
fn synthetic_engine() -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(ConfigBuilder::new().models_dir(dir.path()).normalizer(None).build())
        .expect("an engine starts without its weights");
    (dir, engine)
}

/// Register a seat as live, the way a started session is.
fn live_session(ws: &crate::Workspace, session: &SessionSlot) {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    ws.command_senders.lock().insert(session.clone(), cmd_tx);
    // Kept alive for the test's length: a dropped receiver marks the
    // session dead and every start would refuse.
    std::mem::forget(cmd_rx);
}

/// Put a live take on the runtime the way a registration does, and hand
/// back its stop channel so a test can watch what arrives.
fn live_take(ws: &crate::Workspace, owner: &SessionSlot) -> tokio::sync::mpsc::Receiver<bool> {
    let (stop_tx, stop_rx) = tokio::sync::mpsc::channel(1);
    ws.dictate
        .runtime
        .lock()
        .recordings
        .insert(owner.clone(), LiveRecording { stop: stop_tx, sink: None, initiator: None });
    stop_rx
}

/// The host's app-support surface is the workspace's own field, and the
/// preflight reads the host rather than taking a path of its own. A move
/// that passed a different path - or `None` - leaves every other test here
/// green, so this pins the seam itself.
#[test]
fn the_host_reports_the_workspaces_app_support_dir() {
    let (ws, _updates) = crate::Workspace::testing_stub();
    let field = ws.app_support.clone().expect("the stub sets an app-support base");
    assert_eq!(
        crate::dictate::DictateHost::app_support_dir(ws.as_ref()),
        Ok(field),
        "the host's app-support answer is the workspace's own base, not a second resolution",
    );
}

/// The other app-support branch: a workspace that resolved no base answers
/// the boot failure it recorded, so the preflight's record carries the cause
/// rather than a bare absence.
#[test]
fn the_host_reports_the_recorded_app_support_failure() {
    let cause = "no resolvable data/cache/home dir".to_owned();
    assert_eq!(
        app_support_answer(None, Some(&cause)),
        Err("no resolvable data/cache/home dir".to_owned()),
    );
    assert_eq!(
        app_support_answer(None, None),
        Err("the workspace holds no app-support directory".to_owned()),
        "a workspace with neither a base nor a recorded cause still answers a reason",
    );
    let base = std::path::PathBuf::from("/tmp/base");
    assert_eq!(app_support_answer(Some(&base), None), Ok(base.clone()));
}

/// The progress leg of the host mapping: a take's window crosses to the
/// wire with its done/total and the initiator that ordered it.
#[test]
fn a_progress_update_crosses_to_the_wire_with_its_window() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let seat = key("progress");
    let emitted = crate::dictate::DictateHost::emit(
        ws.as_ref(),
        crate::dictate::DictateUpdate::Progress {
            key: seat.clone(),
            generation: 3,
            done: 2,
            total: Some(5),
            initiator: Some(9),
        },
    );
    assert!(emitted, "the stub holds an answering subscriber");
    let frame = updates.try_recv().expect("the progress frame reaches the stream");
    match &*frame.update {
        SessionUpdate::DictateProgress { key, generation, done, total, initiator } => {
            assert_eq!(key, &seat);
            assert_eq!(*generation, 3);
            assert_eq!(*done, 2);
            assert_eq!(*total, Some(5));
            assert_eq!(*initiator, Some(9));
        }
        other => panic!("the progress update crossed as something else: {other:?}"),
    }
}

#[test]
fn a_set_override_lands_on_its_own_session_and_echoes_back() {
    let (workspace, mut updates) = crate::Workspace::testing_stub();
    let a = crate::SessionSlot::from_str_for_test("dictate-a");
    let b = crate::SessionSlot::from_str_for_test("dictate-b");
    workspace.register_domain_session(a.clone(), None);
    workspace.register_domain_session(b.clone(), None);

    workspace
        .dispatch(Command::SetDictateOverride {
            key: a.clone(),
            update: DictateOverrideUpdate::Styling(crate::Styling::Formal),
        })
        .expect("dispatch");

    let a_binding = workspace.domain_session_for(&a).expect("session a");
    assert_eq!(a_binding.lock().dictate_overrides.styling, Some(crate::Styling::Formal),);
    let b_binding = workspace.domain_session_for(&b).expect("session b");
    assert_eq!(b_binding.lock().dictate_overrides, DictateOverrides::default());

    let echo = updates.try_recv().expect("an echo for the session that set it");
    match std::sync::Arc::unwrap_or_clone(echo.update) {
        SessionUpdate::DictateOverrides { key, overrides } => {
            assert_eq!(key, a);
            assert_eq!(overrides.styling, Some(crate::Styling::Formal));
        }
        other => panic!("unexpected update: {other:?}"),
    }
}

#[test]
fn reset_clears_every_axis_at_once() {
    let (workspace, mut updates) = crate::Workspace::testing_stub();
    let key = crate::SessionSlot::from_str_for_test("dictate-reset");
    workspace.register_domain_session(key.clone(), None);

    for update in [
        DictateOverrideUpdate::Styling(crate::Styling::Casual),
        DictateOverrideUpdate::Context(crate::Context::Email),
    ] {
        workspace
            .dispatch(Command::SetDictateOverride { key: key.clone(), update })
            .expect("dispatch");
    }
    workspace.dispatch(Command::ResetDictateOverrides { key: key.clone() }).expect("dispatch");

    let binding = workspace.domain_session_for(&key).expect("session");
    assert_eq!(binding.lock().dictate_overrides, DictateOverrides::default());

    let mut echoes = 0;
    let mut last = None;
    while let Ok(frame) = updates.try_recv() {
        if matches!(&*frame.update, SessionUpdate::DictateOverrides { .. }) {
            echoes += 1;
            last = Some(std::sync::Arc::unwrap_or_clone(frame.update));
        }
    }
    assert_eq!(echoes, 3, "each set echoes, and the reset does too");

    // **The cleared set is a PRESENT object of nulls, not silence**: a skip
    // on an empty set would leave a client's stale set standing on every
    // reset, with both stacks green.
    let frame = serde_json::to_value(last.expect("the reset's own echo")).expect("serialise");
    assert!(
        frame["dictate_overrides"].get("overrides").is_some(),
        "the reset echo must carry its overrides key, got: {frame}"
    );
}

#[test]
fn an_override_for_an_unknown_session_is_refused() {
    let (workspace, _updates) = crate::Workspace::testing_stub();
    let key = crate::SessionSlot::from_str_for_test("never-registered");
    let err = workspace
        .dispatch(Command::ResetDictateOverrides { key })
        .expect_err("an unknown session must be refused");
    assert!(matches!(err, crate::DispatchError::UnknownSession(_)));
}

/// A stream take registers SYNCHRONOUSLY: the frame a client sends
/// right after its start message, on the one ordered connection,
/// must find the take already there. The registration is what the
/// dispatch did, so nothing is awaited between the two.
#[tokio::test]
async fn a_stream_take_registers_synchronously_and_keeps_its_frames() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");

    assert!(
        ws.dictate_push(&session, &[0.5; 320], Some(1)),
        "the first frame after a start must find its take"
    );

    let started = updates.recv().await.expect("the start echoes");
    assert!(
        matches!(&*started.update, SessionUpdate::DictateStarted { initiator: Some(1), .. }),
        "the start echo carries the connection the take belongs to, got {started:?}"
    );

    // The meter reads the pushed audio.
    let level = tokio::time::timeout(Duration::from_secs(2), updates.recv())
        .await
        .expect("a level must arrive while the take is live")
        .expect("the stream stays open");
    match std::sync::Arc::unwrap_or_clone(level.update) {
        SessionUpdate::DictateLevel { peak_db, .. } => assert!(
            (peak_db + 6.02).abs() < 0.5,
            "the meter must read the pushed samples, got {peak_db} dB"
        ),
        other => panic!("expected a level, got {other:?}"),
    }

    // And the take resolves through the same pipeline: stop submits,
    // and a weightless engine answers the load failure.
    ws.dispatch(Command::DictateStop { key: session.clone(), submit: true, initiator: Some(1) })
        .expect("dispatch");
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { outcome, .. }) => break outcome,
                Some(_) => {}
                None => panic!("the update stream closed before the take resolved"),
            }
        }
    })
    .await
    .expect("the take must resolve");
    assert_eq!(ended, DictateOutcome::Failed, "a weightless engine fails the take");
    assert!(
        ws.dictate.runtime.lock().recordings.is_empty(),
        "a resolved take leaves no live entry behind"
    );
}

/// **The readings and the submit name the take they belong to** (#839).
/// A reader drops a frame that is not its live take's, so a constant or a
/// stale number here leaves the meter and the phase unheard - silently,
/// because every frame still arrives.
#[tokio::test]
async fn the_readings_and_the_submit_carry_the_takes_generation() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("generation");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    ws.dictate_push(&session, &[0.5; 320], Some(1));

    let started = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = updates.recv().await.expect("the stream stays open");
            if let SessionUpdate::DictateStarted { generation, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                break generation;
            }
        }
    })
    .await
    .expect("the take announces itself");

    let level = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = updates.recv().await.expect("the stream stays open");
            if let SessionUpdate::DictateLevel { generation, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                break generation;
            }
        }
    })
    .await
    .expect("a level arrives while the take is live");

    ws.dispatch(Command::DictateStop { key: session.clone(), submit: true, initiator: Some(1) })
        .expect("dispatch");
    let transcribing = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = updates.recv().await.expect("the stream stays open");
            match std::sync::Arc::unwrap_or_clone(frame.update) {
                SessionUpdate::DictateTranscribing { generation, .. } => break generation,
                SessionUpdate::DictateEnded { .. } => {
                    panic!("the take ended before it said it was transcribing")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the take submits");

    assert_eq!(level, started, "a level names the take it belongs to");
    assert_eq!(transcribing, started, "and so does the frame that submits it");
}

/// **The surface a client names on its take reaches the ending update.**
/// The record keeps it with the landed words and hands them back to that
/// page alone, so a seam that drops it here sends them to the wrong one.
#[tokio::test]
async fn a_stream_takes_surface_reaches_its_end() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: Some("review".to_owned()),
        initiator: Some(1),
    })
    .expect("dispatch");
    let started = tokio::time::timeout(Duration::from_secs(2), updates.recv())
        .await
        .expect("the start echoes")
        .expect("the stream stays open");
    assert!(
        matches!(&*started.update, SessionUpdate::DictateStarted { .. }),
        "the take registers, got {started:?}"
    );

    ws.dispatch(Command::DictateStop { key: session.clone(), submit: false, initiator: Some(1) })
        .expect("dispatch");
    let carried = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { surface, .. }) => break surface,
                Some(_) => {}
                None => panic!("the update stream closed before the take resolved"),
            }
        }
    })
    .await
    .expect("the take must resolve");
    assert_eq!(
        carried.as_deref(),
        Some("review"),
        "the ending names the surface the client started the take with"
    );
}

/// **The submitted take's ending carries the surface too.** The final send
/// is the one a landed take uses, and the one the composer folds into its
/// record, so a seam that drops it there loses the words' page.
#[tokio::test]
async fn a_submitted_takes_end_carries_its_surface() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: Some("review".to_owned()),
        initiator: Some(1),
    })
    .expect("dispatch");
    let started = tokio::time::timeout(Duration::from_secs(2), updates.recv())
        .await
        .expect("the start echoes")
        .expect("the stream stays open");
    assert!(
        matches!(&*started.update, SessionUpdate::DictateStarted { .. }),
        "the take registers, got {started:?}"
    );
    assert!(
        ws.dictate_push(&session, &[0.5; 320], Some(1)),
        "the frame after a start must find its take"
    );

    ws.dispatch(Command::DictateStop { key: session.clone(), submit: true, initiator: Some(1) })
        .expect("dispatch");
    let carried = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { surface, .. }) => break surface,
                Some(_) => {}
                None => panic!("the update stream closed before the take resolved"),
            }
        }
    })
    .await
    .expect("the take must resolve");
    assert_eq!(
        carried.as_deref(),
        Some("review"),
        "the submitted ending names the surface the client started the take with"
    );
}

/// One take's recognition failing is that session's own work rather
/// than a warning about forge, so the record is a `debug` - rule 20's
/// split - and it names the seat, which is what a reader acts on.
/// Both halves are the contract: re-raising the level, or dropping
/// the seat, silently undoes the call this pins.
#[tokio::test]
async fn a_failed_take_records_debug_with_its_seat() {
    use tracing_subscriber::layer::SubscriberExt as _;

    /// One record: its level, its target, and its fields.
    #[derive(Clone, Default)]
    struct Caught(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String, String)>>>);

    #[derive(Default)]
    struct Fields(String);

    impl tracing::field::Visit for Fields {
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
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0.lock().expect("capture").push((
                *event.metadata().level(),
                event.metadata().target().to_owned(),
                fields.0,
            ));
        }
    }

    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("solo");
    live_session(&ws, &session);

    // A take runs on a spawned task, and `#[tokio::test]` is a
    // current-thread runtime, so a thread-local default still catches
    // every record it emits.
    let caught = Caught::default();
    let subscriber = tracing_subscriber::registry().with(caught.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    // Without audio the take resolves as `NoAudio` before it ever
    // reaches the engine, so the frame is what puts it on the
    // failure path this test pins.
    assert!(
        ws.dictate_push(&session, &[0.5; 320], Some(1)),
        "the frame after a start must find its take"
    );
    ws.dispatch(Command::DictateStop { key: session.clone(), submit: true, initiator: Some(1) })
        .expect("dispatch");

    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { outcome, .. }) => break outcome,
                Some(_) => {}
                None => panic!("the update stream closed before the take resolved"),
            }
        }
    })
    .await
    .expect("the take must resolve");
    assert_eq!(ended, DictateOutcome::Failed, "a weightless engine fails the take");

    let records = caught.0.lock().expect("capture");
    let (level, target, fields) = records
        .iter()
        .find(|(_, _, fields)| fields.contains("dictate_failed"))
        .unwrap_or_else(|| panic!("a failed take records `dictate_failed`, saw {records:?}"));
    assert_eq!(
        *level,
        tracing::Level::DEBUG,
        "a take's own failure is not a warning about forge: {fields}",
    );
    // The directive that keeps this record readable names a module
    // path, and a target one character off matches nothing, so the
    // path is measured from the record rather than assumed. The module
    // moved into `forge-dictate`, so the target moved with it.
    assert_eq!(
        target.as_str(),
        "forge_dictate::host",
        "the record's target is the module path the filter directive names: {fields}",
    );
    assert!(
        fields.contains("slot=TestOrg/test-project/solo"),
        "the record names the seat it belongs to: {fields}",
    );
}

/// A take's lifecycle is three records, one per transition, in order. They
/// are what a bare `dictate_ended` cannot answer: whether the take ever
/// reached transcribing, and how long each leg took - the question the
/// skipped transcribing row cannot be reconstructed from otherwise.
#[tokio::test]
async fn a_take_records_its_three_transitions_in_order() {
    use tracing_subscriber::layer::SubscriberExt as _;

    /// One record: its level, its target, and its fields.
    #[derive(Clone, Default)]
    struct Caught(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String, String)>>>);

    #[derive(Default)]
    struct Fields(String);

    impl tracing::field::Visit for Fields {
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
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0.lock().expect("capture").push((
                *event.metadata().level(),
                event.metadata().target().to_owned(),
                fields.0,
            ));
        }
    }

    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("traced");
    live_session(&ws, &session);

    // A take runs on a spawned task, and `#[tokio::test]` is a
    // current-thread runtime, so a thread-local default still catches
    // every record it emits.
    let caught = Caught::default();
    let subscriber = tracing_subscriber::registry().with(caught.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(4),
    })
    .expect("dispatch");
    assert!(
        ws.dictate_push(&session, &[0.5; 320], Some(4)),
        "the frame after a start must find its take"
    );
    ws.dispatch(Command::DictateStop { key: session.clone(), submit: true, initiator: Some(4) })
        .expect("dispatch");
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { outcome, .. }) => break outcome,
                Some(_) => {}
                None => panic!("the update stream closed before the take resolved"),
            }
        }
    })
    .await
    .expect("the take must resolve");
    assert_eq!(ended, DictateOutcome::Failed, "a weightless engine fails the take");

    let records = caught.0.lock().expect("capture");
    let transitions: Vec<_> = records
        .iter()
        .filter(|(_, _, fields)| {
            fields.contains("dictate_take_started")
                || fields.contains("dictate_take_transcribing")
                || fields.contains("dictate_take_ended")
        })
        .collect();
    let names: Vec<&str> = transitions
        .iter()
        .map(|(_, _, fields)| {
            if fields.contains("dictate_take_started") {
                "started"
            } else if fields.contains("dictate_take_transcribing") {
                "transcribing"
            } else {
                "ended"
            }
        })
        .collect();
    assert_eq!(
        names,
        ["started", "transcribing", "ended"],
        "one record per transition, in order: {records:?}"
    );
    for (index, (level, target, fields)) in transitions.iter().enumerate() {
        assert_eq!(
            target.as_str(),
            "forge_dictate::host",
            "record {index} comes from the module the filter directive names: {fields}"
        );
        assert!(
            fields.contains("slot=TestOrg/test-project/traced"),
            "record {index} names the seat it belongs to: {fields}"
        );
        assert!(
            fields.contains("generation=1"),
            "record {index} names the take it belongs to: {fields}"
        );
        assert_eq!(
            *level,
            tracing::Level::DEBUG,
            "record {index} is a session's own work at debug, and the cause record owns the level: {fields}"
        );
    }
    assert!(
        transitions[1].2.contains("elapsed_ms="),
        "the transcribing record carries the leg's elapsed time: {}",
        transitions[1].2
    );
    assert!(
        transitions[2].2.contains("outcome=\"failed\"") && transitions[2].2.contains("elapsed_ms="),
        "the ended record carries the outcome and the submit leg's elapsed time: {}",
        transitions[2].2
    );
}

/// A take abandoned before its submit still ends with its own record, and
/// that record is a session's own work at `debug`, like every other ending:
/// the cause record, not this transition, owns any level above it.
#[tokio::test]
async fn a_cancelled_take_records_its_end_at_debug() {
    use tracing_subscriber::layer::SubscriberExt as _;

    #[derive(Clone, Default)]
    struct Caught(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String, String)>>>);

    #[derive(Default)]
    struct Fields(String);

    impl tracing::field::Visit for Fields {
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
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0.lock().expect("capture").push((
                *event.metadata().level(),
                event.metadata().target().to_owned(),
                fields.0,
            ));
        }
    }

    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("abandoned");
    live_session(&ws, &session);

    let caught = Caught::default();
    let subscriber = tracing_subscriber::registry().with(caught.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(4),
    })
    .expect("dispatch");
    assert!(ws.dictate_push(&session, &[0.5; 320], Some(4)), "the take is live");
    assert!(ws.dictate_close(&session, 4), "the connection's own take is closed");
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { outcome, .. }) => break outcome,
                Some(_) => {}
                None => panic!("the update stream closed before the take resolved"),
            }
        }
    })
    .await
    .expect("a closed take must resolve");
    assert_eq!(ended, DictateOutcome::Cancelled, "nothing was submitted");

    // **The absence is the answer.** A take abandoned before its submit
    // never reached transcribing, and the two records that did fire say so:
    // a regression that emits the transcribing record on this path, or loses
    // the started one, would leave the log lying about the skipped state.
    let records = caught.0.lock().expect("capture");
    let transitions: Vec<_> = records
        .iter()
        .filter(|(_, _, fields)| {
            fields.contains("dictate_take_started")
                || fields.contains("dictate_take_transcribing")
                || fields.contains("dictate_take_ended")
        })
        .collect();
    let names: Vec<&str> = transitions
        .iter()
        .map(|(_, _, fields)| {
            if fields.contains("dictate_take_started") {
                "started"
            } else if fields.contains("dictate_take_transcribing") {
                "transcribing"
            } else {
                "ended"
            }
        })
        .collect();
    assert_eq!(
        names,
        ["started", "ended"],
        "an abandoned take records its start and its end and no transcribing: {records:?}"
    );
    let (level, _, fields) = transitions[1];
    assert_eq!(*level, tracing::Level::DEBUG, "a take's own cancel is not a warning: {fields}");
    assert!(
        fields.contains("slot=TestOrg/test-project/abandoned"),
        "the record names the seat it belongs to: {fields}"
    );
    assert!(fields.contains("generation=1"), "and the take it belongs to: {fields}");
    assert!(
        fields.contains("outcome=\"cancelled\""),
        "the record names how the take ended: {fields}"
    );
}

/// A second take on one seat is refused by name, whichever path it
/// came in on: two takes would interleave their notices into one
/// draft with nothing saying why.
#[tokio::test]
async fn a_second_stream_take_on_one_seat_is_refused() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(2),
    })
    .expect("dispatch");

    let first = updates.recv().await.expect("the first start echoes");
    assert!(
        matches!(&*first.update, SessionUpdate::DictateStarted { initiator: Some(1), .. }),
        "got {first:?}"
    );
    let second = updates.recv().await.expect("the refusal echoes");
    match std::sync::Arc::unwrap_or_clone(second.update) {
        SessionUpdate::DictateEnded {
            outcome: DictateOutcome::Refused { message },
            initiator,
            ..
        } => {
            assert_eq!(
                initiator,
                Some(2),
                "the refusal answers the connection that was refused, not the holder"
            );
            assert_eq!(
                message,
                "another client is already dictating on this session · dictation did not start",
                "decision 3's whole deliverable is that wording"
            );
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert_eq!(
        ws.dictate.runtime.lock().recordings.len(),
        1,
        "the refused start must not displace the live take"
    );
}

/// Two seats may stream at once, one take per connection: a take fed by
/// a client's frames holds no device, so nothing about it is exclusive.
#[tokio::test]
async fn two_seats_may_stream_at_once() {
    let (ws, _updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let first = key("first");
    let second = key("second");
    live_session(&ws, &first);
    live_session(&ws, &second);

    for (at, session) in [&first, &second].into_iter().enumerate() {
        ws.dispatch(Command::DictateStream {
            key: session.clone(),
            options: DictateAxes::default(),
            surface: None,
            initiator: Some(at as u64 + 1),
        })
        .expect("dispatch");
        assert!(
            ws.dictate_push(session, &[0.5; 320], Some(at as u64 + 1)),
            "each seat's own take must accept its frames"
        );
    }
    assert_eq!(ws.dictate.runtime.lock().recordings.len(), 2, "both takes are live");
}

/// One live take per connection: a frame carries no seat, so a second
/// take on another seat from the same connection would have no stream
/// its audio could belong to. The refusal rides the same outcome a busy
/// seat's does, so the client that pressed draws it by name.
#[tokio::test]
async fn a_second_take_from_one_connection_is_refused() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let first = key("first");
    let second = key("second");
    live_session(&ws, &first);
    live_session(&ws, &second);

    ws.dispatch(Command::DictateStream {
        key: first.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    ws.dispatch(Command::DictateStream {
        key: second.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");

    let started = updates.recv().await.expect("the first start echoes");
    assert!(
        matches!(&*started.update, SessionUpdate::DictateStarted { initiator: Some(1), .. }),
        "got {started:?}"
    );
    let refusal = updates.recv().await.expect("the refusal echoes");
    match std::sync::Arc::unwrap_or_clone(refusal.update) {
        SessionUpdate::DictateEnded {
            outcome: DictateOutcome::Refused { message },
            initiator,
            ..
        } => {
            assert_eq!(initiator, Some(1), "the refusal answers the client that asked");
            assert_eq!(
                message,
                "this client is already dictating on another seat · dictation did not start",
                "the refusal names the reason in the client's own terms"
            );
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(ws.dictate_push(&first, &[0.5; 320], Some(1)), "the first take keeps its frames");
    assert_eq!(ws.dictate.runtime.lock().recordings.len(), 1, "nothing else registered");
}

/// A frame belongs to the take its own connection started: a seat whose
/// live take is ANOTHER connection's keeps nothing of it, which is
/// decision 3's own scenario - a refused start still has its microphone
/// open for the frames already on the wire.
#[tokio::test]
async fn a_frame_from_another_connection_never_lands_in_this_take() {
    let (ws, _updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");

    assert!(
        !ws.dictate_push(&session, &[0.5; 320], Some(2)),
        "another connection's frames must not land in this take"
    );
    assert!(ws.dictate_push(&session, &[0.5; 320], Some(1)), "its own connection's do");
}

/// A frame for a seat with no live take is dropped rather than kept:
/// the client may be racing a refusal it has not seen, or a take that
/// already resolved.
#[tokio::test]
async fn frames_for_a_seat_with_no_live_take_are_dropped() {
    let (ws, _updates) = crate::Workspace::testing_stub();
    assert!(
        !ws.dictate_push(&key("nobody"), &[0.5; 320], Some(1)),
        "a frame with no take to hold it must be dropped"
    );
}

/// A stream take with no engine refuses with the way out, and the
/// refusal resolves no take, so a client that never saw a start is
/// told why.
#[tokio::test]
async fn a_stream_take_without_an_engine_refuses_with_the_way_out() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");

    let ended = updates.recv().await.expect("the refusal echoes");
    match std::sync::Arc::unwrap_or_clone(ended.update) {
        SessionUpdate::DictateEnded { outcome: DictateOutcome::Refused { message }, .. } => {
            assert!(
                message.contains("not ready"),
                "the refusal must say dictation is not ready, got: {message}"
            );
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(ws.dictate.runtime.lock().recordings.is_empty(), "nothing may be left live");
}

/// A stream take for a seat with no session is refused: the frames would
/// have nowhere to land, and the refusal names the way out. The device
/// path's twin of this has pinned it since the beginning; a stream take
/// must not be the path without cover.
#[tokio::test]
async fn a_stream_take_for_a_seat_with_no_session_is_refused() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    // Deliberately NOT marked live.
    let session = key("ghost");

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");

    let ended = updates.recv().await.expect("the refusal echoes");
    match std::sync::Arc::unwrap_or_clone(ended.update) {
        SessionUpdate::DictateEnded { outcome: DictateOutcome::Refused { message }, .. } => {
            assert!(
                message.contains("closed"),
                "the refusal must say the seat is gone, got: {message}"
            );
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(ws.dictate.runtime.lock().recordings.is_empty(), "nothing may be left live");
    assert!(
        !ws.dictate_push(&session, &[0.5; 320], Some(1)),
        "and nothing may accept frames for a seat no take was registered on"
    );
}

/// A stream take nobody stops ends itself at the cap. A stalled stream
/// has no callback loop to notice the cap for it, so the runner counts
/// the clock - without which the take would hold the seat until teardown.
#[tokio::test]
async fn a_stream_take_nobody_stops_ends_at_its_cap() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let dir = tempfile::tempdir().unwrap();
    let cfg = ConfigBuilder::new()
        .models_dir(dir.path())
        .normalizer(None)
        .max_capture(Duration::from_secs(1))
        .build();
    let engine = Engine::new(cfg).expect("engine must start");
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("capped");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    assert!(ws.dictate_push(&session, &[0.5; 320], Some(1)), "the take is live");
    let _ = updates.recv().await.expect("the start echoes");

    // Nothing else stops it: no release, no drop, no further frame.
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { .. }) => break true,
                Some(_) => {}
                None => break false,
            }
        }
    })
    .await
    .expect("the cap must end a take nobody stops");
    assert!(ended, "the take must resolve rather than hold the seat forever");
    assert!(ws.dictate.runtime.lock().recordings.is_empty(), "and the seat is free again");
}

/// The connection going away DROPS the take it was streaming and frees
/// the seat AT ONCE: nothing it streamed lands anywhere - its reader is
/// gone - and a client that reconnects and starts again must not be
/// refused by the take it left behind.
#[tokio::test]
async fn closing_a_stream_take_drops_it_and_frees_the_seat() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    assert!(ws.dictate_push(&session, &[0.5; 320], Some(1)));
    let _ = updates.recv().await.expect("the start echoes");

    assert!(
        !ws.dictate_close(&session, 2),
        "a take another connection started is not this one's to close"
    );
    assert_eq!(
        ws.dictate.runtime.lock().recordings.len(),
        1,
        "and the foreign close leaves it recording"
    );
    assert!(ws.dictate_close(&session, 1), "the live stream take is closed");
    assert!(
        ws.dictate.runtime.lock().recordings.is_empty(),
        "the seat is free before the take resolves, which is what the next start needs"
    );
    assert!(!ws.dictate_close(&session, 1), "closing a second time closes nothing");

    // CANCELLED, not failed: a weightless engine FAILS a submitted
    // take, so the abandoned end is what says nothing was submitted -
    // the drop this path exists for.
    let resolved = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match updates.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                Some(SessionUpdate::DictateEnded { outcome, .. }) => break Some(outcome),
                Some(_) => {}
                None => break None,
            }
        }
    })
    .await
    .expect("a closed take must resolve rather than hold the seat");
    assert!(
        matches!(resolved, Some(DictateOutcome::Cancelled)),
        "a dropped take must be abandoned with nothing submitted, got {resolved:?}"
    );

    // The property the refusal broke: the next take registers at once.
    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    assert!(
        ws.dictate_push(&session, &[0.5; 320], Some(1)),
        "a take started right after a drop must land"
    );
}

/// A stop belongs to the connection whose take it addresses: a stop
/// from any other connection parks for its own connection rather than
/// reaching this take, so a view's dictation is not killed by a stop
/// that never owned it.
#[tokio::test]
async fn a_stop_from_another_connection_leaves_the_take_running() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    let session = key("streamer");
    live_session(&ws, &session);

    ws.dispatch(Command::DictateStream {
        key: session.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: Some(1),
    })
    .expect("dispatch");
    let _ = updates.recv().await.expect("the start echoes");

    // Awaited rather than dispatched: the dispatch arm spawns the
    // handler, so an assertion right after it reads before the handler
    // has run. The park is this property's own statement - the stop
    // found no take of its own and left one for a start that never
    // comes - and it is settled the moment the handler returns.
    handle_dictate_stop(&ws.dictate, &session, false, Some(2)).await;
    assert!(
        matches!(
            &ws.dictate.runtime.lock().stop_pending,
            Some((parked, who, _)) if parked == &session && *who == Some(2)
        ),
        "a foreign stop parks for its own connection rather than reaching this take"
    );
    assert!(
        ws.dictate_push(&session, &[0.5; 320], Some(1)),
        "a foreign stop must not end the take"
    );
    assert_eq!(ws.dictate.runtime.lock().recordings.len(), 1, "and the take is still live");
}

/// A take already submitted is still its connection's to cancel: the
/// transcript would have no reader left, so the close abandons it - and
/// another connection's close is not this take's at all.
#[tokio::test]
async fn closing_a_finishing_take_abandons_its_transcript() {
    let (ws, _updates) = crate::Workspace::testing_stub();
    let session = key("finishing");
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
    ws.dictate.runtime.lock().finishing.push(FinishingTake {
        key: session.clone(),
        stop: stop_tx,
        initiator: Some(1),
    });

    assert!(!ws.dictate_close(&session, 2), "another connection's close is not this take's");
    assert_eq!(
        ws.dictate.runtime.lock().finishing.len(),
        1,
        "and a foreign close leaves the take awaiting its transcript"
    );
    assert!(ws.dictate_close(&session, 1), "the submitted take is its owner's to cancel");
    assert_eq!(
        stop_rx.try_recv(),
        Ok(false),
        "the close abandons the transcript rather than submitting it"
    );
}

/// Teardown drops a session's park: an inert entry for a closed
/// session would otherwise sit in the Option forever.
#[tokio::test]
async fn closing_the_session_clears_its_parked_stop() {
    let (ws, _updates) = crate::Workspace::testing_stub();
    let owner = key("owner");
    handle_dictate_stop(&ws.dictate, &owner, true, None).await;
    assert!(ws.dictate.runtime.lock().stop_pending.is_some());

    ws.release_session(&owner);

    assert_eq!(ws.dictate.runtime.lock().stop_pending, None);
}

/// A refused start clears the key's park: the release that follows
/// the refusal parks AFTER this clear, and that later park is
/// handled by the freshness window, but a park that arrived before
/// the refusal must not survive it.
#[tokio::test]
async fn a_refused_start_clears_the_park() {
    let (ws, mut updates) = crate::Workspace::testing_stub();
    let owner = key("owner");
    ws.dictate.runtime.lock().stop_pending = Some((owner.clone(), None, Instant::now()));

    // No engine on the stub, so the registration refuses.
    ws.dispatch(Command::DictateStream {
        key: owner.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: None,
    })
    .expect("dispatch");

    assert_eq!(ws.dictate.runtime.lock().stop_pending, None);
    let ended = updates.recv().await.expect("the refusal echo");
    assert!(matches!(
        std::sync::Arc::unwrap_or_clone(ended.update),
        SessionUpdate::DictateEnded { outcome: DictateOutcome::Refused { .. }, .. }
    ));
}

/// Closing the session that owns a live take must release it and cut
/// its channel - otherwise the take stays live and the meter keeps
/// emitting for a composer that no longer exists, and every later
/// start is refused naming a session the user closed.
#[tokio::test]
async fn closing_the_session_releases_its_live_take() {
    let (ws, _updates) = crate::Workspace::testing_stub();
    let owner = key("owner");
    let mut stop_rx = live_take(&ws, &owner);
    let (finishing_tx, _finishing_rx) = tokio::sync::mpsc::channel(1);
    ws.dictate.runtime.lock().finishing.push(FinishingTake {
        key: owner.clone(),
        stop: finishing_tx,
        initiator: None,
    });

    ws.release_session(&owner);

    {
        let runtime = ws.dictate.runtime.lock();
        assert!(runtime.recordings.is_empty(), "a closed session must not keep a live take");
        assert!(
            runtime.finishing.is_empty(),
            "a closed session's submitted take must not keep a cancel route"
        );
    }
    assert!(
        matches!(stop_rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)),
        "dropping the entry must close the channel, which is what makes the runner abandon the take"
    );
}

/// The seam's own witness. A workspace built from a config that turns
/// dictation on registers the subsystem from that config, and a command
/// dispatched through the bus reaches the subsystem and comes back as the
/// frame a client reads. A move that leaves the subsystem unregistered,
/// or wires a command to a handle nothing renders, leaves every other
/// test in this file green and this one red. This pins the dispatch leg;
/// the emit return leg is pinned by
/// `a_progress_update_crosses_to_the_wire_with_its_window`.
#[tokio::test]
async fn the_subsystem_registers_from_config_and_a_command_round_trips() {
    let (ws, mut updates) = crate::Workspace::testing_stub_with_dictate_enabled();
    let owner = key("seam");
    live_session(&ws, &owner);

    assert!(
        !ws.dictate.state.snapshot.lock().models.is_empty(),
        "the enabled config must reach the subsystem as its model rows"
    );

    let (_dir, engine) = synthetic_engine();
    *ws.dictate.state.engine.lock() = Some(Arc::clone(&engine));
    ws.dispatch(Command::DictateStream {
        key: owner.clone(),
        options: DictateAxes::default(),
        surface: None,
        initiator: None,
    })
    .expect("dispatch");

    let echo = tokio::time::timeout(std::time::Duration::from_secs(30), updates.recv())
        .await
        .expect("the start reaches the update stream")
        .expect("the seat stream stays open");
    assert!(
        matches!(
            std::sync::Arc::unwrap_or_clone(echo.update),
            SessionUpdate::DictateStarted { .. }
        ),
        "the subsystem must answer a command through the workspace's own update sender"
    );
}
