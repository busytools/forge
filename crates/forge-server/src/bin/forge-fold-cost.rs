//! A forge to measure against, and the client that measures it.
//!
//! The cost this instrument exists for is a cost to a PROCESS, so the
//! measurement is `ps -o time=` on a forge's own CPU time, sampled around a
//! window of raw websocket work and charged against an idle arm of the same
//! length. That needs a forge the instrument can watch from outside without
//! disturbing the one somebody is using, and it needs the client in another
//! process - a client sharing this one would put its own decoding of a 28 MB
//! frame inside the number.
//!
//! Two roles, and the server is the default:
//!
//! ```text
//! forge-fold-cost --transcript /tmp/hub.jsonl --breakdown      # serve
//! forge-fold-cost --measure --pid <that pid> --arm refresh    # measure
//! ```
//!
//! **Every arm prints its own denominator** - the frames it read, the bytes
//! they carried, the turns and messages they held - because an arm that
//! reported a cost without them could not be told from one that had stopped
//! seeing the conversation at all. The check that matters is running two arms
//! of the same name against a server before and after a change and finding
//! the bytes identical: that is what says the instrument was still watching
//! the conversation rather than having gone quiet.
//!
//! **And each arm sends what its name says**, which is not a detail. A
//! `refresh` is an `unsubscribe` and a `subscribe`, because that is what the
//! client sends on every update for the open seat; a `subscribe` carries its
//! own connect; an `idle` arm does nothing at all. An arm that sent a `more`
//! and called itself a refresh would report on a path the client never takes.
//!
//! The seat's conversation is the file `--transcript` names, written in under
//! the session id the fixture seeds. Nothing here synthesises one: a fold over
//! invented rows is a fold over rows whose size is a property of this file
//! rather than of a conversation, which is the one thing a measurement of
//! per-byte cost cannot afford.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use forge_primitives::SessionSlot;
use forge_server::live::Live;
use forge_server::surface::ViewSurface;
use forge_server::testing::Fleet;
use forge_server::transport::TransportState;
use forge_server::transport::envelope::{ClientMessage, ServerMessage, Subject};
use forge_server::transport::serve;
use forge_server::work::WorkCache;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
struct Args {
    /// A session transcript to serve as the seat's conversation, read whole.
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// The org the fixture declares.
    #[arg(long, default_value = "Bench")]
    org: String,
    /// The project the fixture declares. Its directory is under `--dir`.
    #[arg(long, default_value = "seat")]
    project: String,
    /// Where the fixture's config dir goes. It is the workspace's own store,
    /// so it is wiped and rewritten on every boot.
    #[arg(long, default_value = "/tmp/forge-fold-cost")]
    dir: PathBuf,
    /// The port. Fixed rather than ephemeral: a harness that samples `ps`
    /// needs to find the server by something, and the port is what survives a
    /// PID nobody wrote down.
    #[arg(long, default_value_t = 8791)]
    port: u16,
    /// Report where the time goes inside one request before serving.
    #[arg(long)]
    breakdown: bool,
    /// Measure a running server rather than serve one.
    #[arg(long)]
    measure: bool,
    /// The server's PID, which is whose CPU time the measurement charges.
    #[arg(long)]
    pid: Option<u32>,
    /// The arm to run, and each one sends what its name says:
    /// `idle` does nothing and is the baseline; `subscribe` is one connect
    /// and one subscribe; `more` is an attach then `--asks` pages; and
    /// `refresh` is an attach then `--asks` subscribe-reloads, each an
    /// `unsubscribe` and a `subscribe`, which is what the client sends on
    /// every update.
    #[arg(long, default_value = "refresh")]
    arm: String,
    /// How many turns each `more` asks for.
    #[arg(long, default_value_t = 20)]
    turns: u32,
    /// How many asks an arm makes.
    #[arg(long, default_value_t = 8)]
    asks: u32,
    /// How long the idle arm runs, and the wall the charge is scaled against.
    #[arg(long, default_value_t = 2.0)]
    seconds: f64,
}

/// This process's CPU time, or another's, at `ps`'s 10 ms resolution.
fn cpu_seconds(pid: u32) -> anyhow::Result<f64> {
    let out =
        std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "time="]).output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<f64>().unwrap_or(0.0), rest),
        None => (0.0, text),
    };
    let mut parts: Vec<f64> =
        rest.split(':').map(|part| part.parse::<f64>().unwrap_or(0.0)).collect();
    while parts.len() < 3 {
        parts.insert(0, 0.0);
    }
    Ok(days * 86400.0 + parts[0] * 3600.0 + parts[1] * 60.0 + parts[2])
}

/// This process's resident set, which is what holding a conversation costs.
fn rss() -> String {
    std::process::Command::new("ps")
        .args(["-p", &std::process::id().to_string(), "-o", "rss="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// What one arm moved, and what it cost.
///
/// The denominator is not decoration: it is what tells a reader that the arm
/// saw a conversation at all.
#[derive(Default)]
struct Arm {
    cpu_ms: f64,
    wall_ms: f64,
    frames: u64,
    bytes: u64,
    turns: u64,
    messages: u64,
    asks: u32,
}

impl Arm {
    fn report(&self, arm: &str, idle: &Arm) -> String {
        let share = if idle.wall_ms > 0.0 { self.wall_ms / idle.wall_ms } else { 0.0 };
        format!(
            "{{\"arm\": \"{arm}\", \"charged_cpu_ms\": {:.1}, \"wall_ms\": {:.1}, \
             \"frames\": {}, \"bytes\": {}, \"turns\": {}, \"messages\": {}, \"asks\": {}, \
             \"idle_cpu_ms\": {:.1}, \"idle_share\": {:.3}}}",
            self.cpu_ms - idle.cpu_ms * share,
            self.wall_ms,
            self.frames,
            self.bytes,
            self.turns,
            self.messages,
            self.asks,
            idle.cpu_ms,
            share,
        )
    }
}

/// The messages a page or a snapshot carried, counted the way a client draws
/// them: one per turn, plus the frames each turn holds.
fn count(payload: &serde_json::Value) -> (u64, u64) {
    let Some(turns) = payload.get("turns").and_then(|turns| turns.as_array()) else {
        return (0, 0);
    };
    let messages = turns
        .iter()
        .map(|turn| turn.get("messages").and_then(|m| m.as_array()).map_or(0, |m| m.len() as u64))
        .sum();
    (turns.len() as u64, messages)
}

/// A socket with nothing asked on it yet, which the greeting has arrived on.
///
/// A whole-conversation snapshot is tens of megabytes, past the client's
/// default message size: a client that refused it would measure the refusal
/// rather than the encode.
async fn a_socket(
    args: &Args,
) -> anyhow::Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let url = format!("ws://127.0.0.1:{}/socket", args.port);
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(None)
        .max_frame_size(None);
    let (socket, _) =
        tokio_tungstenite::connect_async_with_config(url, Some(config), false).await?;
    Ok(socket)
}

/// Attach to the seat and take the answer, which is what a connect costs.
///
/// Nothing here is counted into an arm: this is the price of arriving, and
/// the arm it precedes is the price of staying.
async fn attach(
    args: &Args,
    seat: &SessionSlot,
) -> anyhow::Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let mut socket = a_socket(args).await?;
    let opening = ClientMessage::Subscribe {
        what: Subject::Session(seat.clone()),
        // Not answering: registering as one that can answer parks a turn on
        // this client's reply, and a measurement must not change the thing it
        // measures.
        answering: false,
    };
    socket.send(Message::Text(serde_json::to_string(&opening)?.into())).await?;
    let mut uncounted = Arm::default();
    let _ = read_one(&mut socket, &mut uncounted).await?;
    Ok(socket)
}

/// Ask on a connection already open, reading every frame and counting it.
async fn work(
    args: &Args,
    seat: &SessionSlot,
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    arm: &mut Arm,
) -> anyhow::Result<()> {
    let mut before = None;
    let asks = if args.arm == "subscribe" { 1 } else { args.asks };
    for _ in 0..asks {
        let asked = match args.arm.as_str() {
            "subscribe" => {
                ClientMessage::Subscribe { what: Subject::Session(seat.clone()), answering: false }
            }
            // The client's own order: let the seat go, then ask for it again.
            // Nothing answers an unsubscribe, so the read below waits for the
            // subscribe's snapshot.
            "refresh" => {
                let let_go = ClientMessage::Unsubscribe { what: Subject::Session(seat.clone()) };
                socket.send(Message::Text(serde_json::to_string(&let_go)?.into())).await?;
                ClientMessage::Subscribe { what: Subject::Session(seat.clone()), answering: false }
            }
            _ => ClientMessage::More {
                conversation: seat.clone(),
                before: before.clone(),
                turns: args.turns,
            },
        };
        socket.send(Message::Text(serde_json::to_string(&asked)?.into())).await?;
        if let ServerMessage::Page { cursor, .. } = read_one(socket, arm).await? {
            before = cursor;
        }
    }
    Ok(())
}

/// The next answer on the socket, counted into `arm`.
///
/// The greeting and anything a live core pushes while this runs are read and
/// passed over rather than counted: an arm's denominator is what the ask it
/// made was answered with, not everything the socket happened to say.
async fn read_one(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    arm: &mut Arm,
) -> anyhow::Result<ServerMessage> {
    loop {
        let Some(message) = socket.next().await else {
            anyhow::bail!("the server stopped talking before it answered");
        };
        let Message::Text(text) = message? else {
            continue;
        };
        match serde_json::from_str::<ServerMessage>(&text)? {
            answer @ (ServerMessage::Snapshot { .. } | ServerMessage::Page { .. }) => {
                arm.frames += 1;
                arm.bytes += text.len() as u64;
                let payload = match &answer {
                    ServerMessage::Snapshot { data, .. } => data["conversation"].clone(),
                    ServerMessage::Page { turns, .. } => serde_json::json!({ "turns": turns }),
                    _ => serde_json::Value::Null,
                };
                let (turns, messages) = count(&payload);
                arm.turns += turns;
                arm.messages += messages;
                return Ok(answer);
            }
            ServerMessage::Error { what, why } => anyhow::bail!("{what} refused: {why}"),
            // The greeting, and anything a live core pushes while this runs.
            // Skipped rather than returned: a caller waiting for its answer
            // would otherwise take the greeting as one and read every later
            // answer a frame behind.
            _ => {}
        }
    }
}

async fn measure(args: &Args) -> anyhow::Result<()> {
    let Some(pid) = args.pid else {
        anyhow::bail!("--measure wants --pid, which is whose CPU time the arms are charged to");
    };
    let seat = SessionSlot::lead(&args.org, &args.project);

    let mut idle = Arm::default();
    let started = Instant::now();
    let before = cpu_seconds(pid)?;
    tokio::time::sleep(Duration::from_secs_f64(args.seconds)).await;
    idle.wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    idle.cpu_ms = (cpu_seconds(pid)? - before) * 1000.0;

    // Nothing to do for `idle`: the bracket above is the whole arm, and it is
    // the baseline every other arm is charged against rather than a workload
    // of its own.
    if args.arm == "idle" {
        println!("{}", Arm::default().report("idle", &idle));
        return Ok(());
    }

    // `subscribe` is the price of arriving, so it is NOT attached first and
    // its own connect is inside the bracket. Every other arm measures the
    // price of staying, so its attach is taken before the bracket - charging
    // a connect to a refresh would report arriving as staying.
    let mut socket = match args.arm.as_str() {
        "subscribe" => a_socket(args).await?,
        _ => attach(args, &seat).await?,
    };
    let mut arm =
        Arm { asks: if args.arm == "subscribe" { 1 } else { args.asks }, ..Arm::default() };
    let started = Instant::now();
    let before = cpu_seconds(pid)?;
    work(args, &seat, &mut socket, &mut arm).await?;
    arm.wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    arm.cpu_ms = (cpu_seconds(pid)? - before) * 1000.0;

    println!("{}", arm.report(&args.arm, &idle));
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.measure {
        return measure(&args).await;
    }
    let Some(transcript) = args.transcript.as_deref() else {
        anyhow::bail!("serving wants --transcript, or --measure to watch another server");
    };
    serve_one(&args, transcript).await
}

async fn serve_one(args: &Args, transcript: &Path) -> anyhow::Result<()> {
    let rows = std::fs::read_to_string(transcript)?;
    let rows: Vec<&str> = rows.lines().filter(|row| !row.trim().is_empty()).collect();
    let bytes = rows.iter().map(|row| row.len() + 1).sum::<usize>();

    std::fs::create_dir_all(&args.dir)?;
    let fleet = Fleet::in_dir(&args.dir, &[(&args.org, &[args.project.as_str()])])
        .map_err(anyhow::Error::msg)?;
    fleet.start(&args.org, &args.project).map_err(anyhow::Error::msg)?;
    fleet.seed_transcript(&args.org, &args.project, "lead", &rows).map_err(anyhow::Error::msg)?;
    let seat = SessionSlot::lead(&args.org, &args.project);

    // The denominator, on stdout before the socket opens. A reader who sees no
    // conversation on the wire cannot tell "the fix works" from "the file was
    // not there", and this is the line that tells them apart.
    println!("seeded {} rows / {} bytes as {}/{}", rows.len(), bytes, args.org, args.project);

    let surface = fleet.surface();
    if args.breakdown {
        println!("{}", breakdown(&surface, &seat, &args.dir.join(&args.project))?);
    }

    let state = Arc::new(TransportState {
        surface,
        work: Arc::new(WorkCache::new()),
        live: Mutex::new(Live::new()),
        config: forge_primitives::WebConfig::default(),
    });
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("pid {} listening on ws://{addr}/socket for {seat:?}", std::process::id());
    serve(state, listener).await?;
    Ok(())
}

/// Where the time goes inside one request, taken once before any client
/// attaches: a fix that removes one of these three is worth what that one
/// costs and no more.
fn breakdown(surface: &Arc<ViewSurface>, seat: &SessionSlot, cwd: &Path) -> anyhow::Result<String> {
    let peak = rss();

    let started = Instant::now();
    let read = surface.conversation(seat, cwd);
    let read_ms = started.elapsed().as_millis();
    let holding = rss();

    let started = Instant::now();
    let rendered = forge_server::transcript::render(&read.messages);
    let fold_ms = started.elapsed().as_millis();

    let started = Instant::now();
    let turns = forge_server::transport::wire::all_turns(&read.messages, &rendered.turns);
    let encode_ms = started.elapsed().as_millis();

    Ok(format!(
        "breakdown: read {read_ms} ms, fold {fold_ms} ms, encode {encode_ms} ms over {} \
         messages; resident {peak} KiB with the fleet, {holding} KiB holding the read, {} KiB \
         holding read + fold + encode ({} turns)",
        read.messages.len(),
        rss(),
        turns.len(),
    ))
}
