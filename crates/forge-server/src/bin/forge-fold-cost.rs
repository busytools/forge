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
//! cargo build --release -p forge-server --features testing --bin forge-fold-cost
//! forge-fold-cost --transcript /tmp/hub.jsonl --breakdown      # serve
//! forge-fold-cost --measure --pid <that pid> --arm refresh    # measure
//! ```
//!
//! **Build it in release.** The profile moves the headline figure by 7x -
//! the same transcript and the same command read 1,413 ms for a read in a
//! debug build and 204 ms in a release one - so every row says which build
//! its figures came from. That is the MEASURING CLIENT's build, and the
//! server's is a separate thing a row cannot report: see `client_profile`.
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use forge_primitives::SessionSlot;
use forge_server::surface::ViewSurface;
use forge_server::testing::Fleet;
use forge_server::transport::TransportState;
use forge_server::transport::envelope::{ClientMessage, ServerMessage, Subject};
use forge_server::transport::serve;
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
    /// Where the fixture's config dir goes. The workspace's store lives under
    /// it and the transcript is written into it, so a fresh directory gives a
    /// fresh seat; nothing here clears one, so a rerun over the same directory
    /// reuses the store it left.
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
    ///
    /// **A closed set, so a mistyped arm is refused rather than run as
    /// `more`.** An arm that ran a different workload under the name a
    /// person typed is the defect this instrument exists to find, and it is
    /// the same mistake whether the code or the caller makes it.
    #[arg(long, default_value = "refresh")]
    arm: Arm,
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

/// Another process's CPU time, at `ps`'s 10 ms resolution.
///
/// **A PID that cannot be read is an error, never a zero.** A stale PID is
/// the ordinary case - the server prints its PID once and a person pastes it
/// after a restart - and a zero here is the strongest false positive this
/// instrument can produce: a full, plausible denominator of bytes and turns
/// beside "the server cost nothing". The denominator exists so an arm that
/// stopped seeing the conversation can be told from one that worked; the CPU
/// half needs the same guard.
fn cpu_seconds(pid: u32) -> anyhow::Result<f64> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "time="])
        .output()
        .map_err(|error| anyhow::anyhow!("ps could not be run for pid {pid}: {error}"))?;
    if !out.status.success() {
        // `ps` says nothing on stderr for a dead PID, which is its common
        // case here, so the sentence carries the status rather than a colon
        // with nothing after it.
        let said = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        anyhow::bail!(
            "ps failed for pid {pid} ({}){}",
            out.status,
            if said.is_empty() { String::new() } else { format!(": {said}") },
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_cpu_time(text.trim())
        .ok_or_else(|| anyhow::anyhow!("ps reported no CPU time for pid {pid}: `{text}`"))
}

/// `[[dd-]hh:]mm:ss.cc` as seconds, or `None` when it is not that.
fn parse_cpu_time(text: &str) -> Option<f64> {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<f64>().ok()?, rest),
        None => (0.0, text),
    };
    let mut parts = rest.split(':').map(str::parse::<f64>).collect::<Result<Vec<f64>, _>>().ok()?;
    if parts.is_empty() {
        return None;
    }
    while parts.len() < 3 {
        parts.insert(0, 0.0);
    }
    Some(days * 86400.0 + parts[0] * 3600.0 + parts[1] * 60.0 + parts[2])
}

/// What this run was built as, which moves the headline figure by 7x.
///
/// Same transcript, same command: a debug build reads 1,413 ms where a
/// release one reads 204, and a debug CLIENT reads 2,695 ms of wall where a
/// release one reads 1,250.
///
/// **This is the MEASURING CLIENT's build, and it is not the server's.** A
/// row's `charged_cpu_ms` is the server's CPU, which moves with the server's
/// own build, and the two can differ: a release client measuring a debug
/// server reports `client_profile: release` about a server built debug. The
/// server's build is announced by its own serve line and is not recoverable
/// from a row - `--pid` is an address, and the greeting carries a protocol
/// version rather than a build.
///
/// **The serve line uses this too, and there the caller IS the server.** It
/// says which build is listening; a row's `client_profile` says which build
/// is measuring. Two processes, and they can disagree.
fn profile() -> &'static str {
    if cfg!(debug_assertions) { "debug" } else { "release" }
}

/// This process's resident set, which is what holding a conversation costs.
fn rss() -> anyhow::Result<u64> {
    let pid = std::process::id();
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "rss="])
        .output()
        .map_err(|error| anyhow::anyhow!("ps could not be run for pid {pid}: {error}"))?;
    if !out.status.success() {
        anyhow::bail!("ps failed for this process's rss ({})", out.status);
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("ps reported no rss for this process"))
}

/// The workload an arm sends, as a closed set.
///
/// **A closed set rather than a string, so a mistyped arm is refused rather
/// than run as something else.** An arm that ran a different workload under
/// the name a person typed is the defect this instrument exists to find, and
/// it is the same mistake whether the code or the caller makes it.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Arm {
    /// Nothing at all, and the baseline every other arm is charged against.
    Idle,
    /// One connect and one subscribe: the price of arriving.
    Subscribe,
    /// An attach, then `--asks` pages.
    More,
    /// An attach, then `--asks` subscribe-reloads, each an `unsubscribe` and
    /// a `subscribe` - what the client sends on every update for the open
    /// seat.
    Refresh,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Subscribe => "subscribe",
            Self::More => "more",
            Self::Refresh => "refresh",
        }
    }
}

/// What one arm moved, and what it cost.
///
/// The denominator is not decoration: it is what tells a reader that the arm
/// saw a conversation at all.
#[derive(Default)]
struct Measured {
    cpu_ms: f64,
    wall_ms: f64,
    frames: u64,
    bytes: u64,
    turns: u64,
    messages: u64,
    asks: u32,
    /// The PID this arm was charged to, echoed from `--pid`.
    ///
    /// **Echoed rather than verified**: nothing here checks that the process
    /// owns the port, so a recycled PID is charged and then named as the
    /// server. It is an address for a reader to check, not a claim.
    pid: u32,
}

impl Measured {
    /// The line one arm reports.
    ///
    /// Every row carries the build it was MEASURED WITH, the PID it charged,
    /// and - on the idle row, which every other arm is charged against - the
    /// window ASKED FOR rather than the one the clock measured, so a reader
    /// recomputing the charge from a lifted row gets a number slightly under
    /// the printed one. The divisor is the idle arm's own wall time, which no
    /// row prints; the gap is milliseconds normally and wider under load,
    /// which is when someone is most likely to be recomputing.
    ///
    /// **The client's build moves two of the figures, and the charge is only
    /// as clean as the baseline.** `wall_ms` is the client's own and a debug
    /// client reads 2,695 ms where a release one reads 1,250 on the same
    /// server. `charged_cpu_ms` is `cpu - idle_cpu * share`, so where the
    /// idle arm reads 0.0 the charge is insensitive to that build; where the
    /// server has background work the baseline is non-zero, `share` moves
    /// with the client's wall, and the charge moves with it.
    fn report(&self, arm: &str, idle: &Measured, window: Option<f64>) -> String {
        let share = if idle.wall_ms > 0.0 { self.wall_ms / idle.wall_ms } else { 0.0 };
        let window = match window {
            Some(seconds) => format!(", \"window_s\": {seconds:.1}"),
            None => String::new(),
        };
        format!(
            "{{\"arm\": \"{arm}\", \"client_profile\": \"{}\", \"charged_pid\": {}, \
             \"charged_cpu_ms\": {:.1}, \"wall_ms\": {:.1}, \"frames\": {}, \"bytes\": {}, \
             \"turns\": {}, \"messages\": {}, \"asks\": {}, \"idle_cpu_ms\": {:.1}, \
             \"idle_share\": {:.3}{window}}}",
            profile(),
            self.pid,
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
        // Not hosting either, for the same reason.
        browser: false,
    };
    socket.send(Message::Text(serde_json::to_string(&opening)?.into())).await?;
    let mut uncounted = Measured::default();
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
    arm: &mut Measured,
) -> anyhow::Result<()> {
    let mut before = None;
    let asks = if args.arm == Arm::Subscribe { 1 } else { args.asks };
    for _ in 0..asks {
        let asked = match args.arm {
            Arm::Subscribe => ClientMessage::Subscribe {
                what: Subject::Session(seat.clone()),
                answering: false,
                browser: false,
            },
            // The client's own order: let the seat go, then ask for it again.
            // Nothing answers an unsubscribe, so the read below waits for the
            // subscribe's snapshot.
            Arm::Refresh => {
                let let_go = ClientMessage::Unsubscribe { what: Subject::Session(seat.clone()) };
                socket.send(Message::Text(serde_json::to_string(&let_go)?.into())).await?;
                ClientMessage::Subscribe {
                    what: Subject::Session(seat.clone()),
                    answering: false,
                    browser: false,
                }
            }
            Arm::Idle | Arm::More => ClientMessage::More {
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
    arm: &mut Measured,
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
            ServerMessage::Error { what, why, .. } => anyhow::bail!("{what} refused: {why}"),
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

    let mut idle = Measured { pid, ..Measured::default() };
    let started = Instant::now();
    let before = cpu_seconds(pid)?;
    tokio::time::sleep(Duration::from_secs_f64(args.seconds)).await;
    idle.wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    idle.cpu_ms = (cpu_seconds(pid)? - before) * 1000.0;

    // Nothing to do for `idle`: the bracket above is the whole arm, and it is
    // the baseline every other arm is charged against rather than a workload
    // of its own. Its window goes on the row because every other arm is
    // charged against it and the row cannot be read without one.
    if args.arm == Arm::Idle {
        println!(
            "{}",
            Measured { pid, ..Measured::default() }.report(
                Arm::Idle.name(),
                &idle,
                Some(args.seconds)
            )
        );
        return Ok(());
    }

    // `subscribe` is the price of ARRIVING, so its socket is opened inside
    // the bracket - a connect outside it would price a subscribe on a socket
    // somebody else had already opened. Every other arm measures the price of
    // staying, so its attach is taken before the bracket: charging a connect
    // to a refresh would report arriving as staying.
    let mut attached = match args.arm {
        Arm::Subscribe => None,
        _ => Some(attach(args, &seat).await?),
    };
    let mut arm = Measured {
        asks: if args.arm == Arm::Subscribe { 1 } else { args.asks },
        pid,
        ..Measured::default()
    };
    let started = Instant::now();
    let before = cpu_seconds(pid)?;
    let mut socket = match attached.take() {
        Some(socket) => socket,
        None => a_socket(args).await?,
    };
    work(args, &seat, &mut socket, &mut arm).await?;
    arm.wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    arm.cpu_ms = (cpu_seconds(pid)? - before) * 1000.0;

    println!("{}", arm.report(args.arm.name(), &idle, None));
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

    // The conditions and the denominator, on stdout before the socket opens.
    // A reader who sees no conversation on the wire cannot tell "the fix
    // works" from "the file was not there" - and a reader who does not know
    // the profile cannot tell a 200 ms figure from a 1,400 ms one.
    println!(
        "built {}; seeded {} rows / {} bytes as {}/{}",
        profile(),
        rows.len(),
        bytes,
        args.org,
        args.project
    );

    let surface = fleet.surface();
    if args.breakdown {
        println!("{}", breakdown(&surface, &seat, &args.dir.join(&args.project))?);
    }

    let state = Arc::new(TransportState::for_workspace(
        &fleet.workspace(),
        forge_primitives::ClientConfig::default(),
    ));
    // The seat's conversation, put where a `Connected` would have left it.
    //
    // **The transport does not read a transcript**, so a server left to
    // itself here would answer every arm from an empty conversation and
    // report the cheapest result of all. The fixture reads the transcript raw
    // and holds what it read - the state a client attaching to a running seat
    // meets.
    fleet
        .hold_conversation(&state, &args.org, &args.project, "lead")
        .map_err(anyhow::Error::msg)?;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("pid {} listening on ws://{addr}/socket for {seat:?}", std::process::id());
    serve(state, listener).await?;
    Ok(())
}

/// Where the CONVERSATION's time goes inside one request, taken once before
/// any client attaches: a fix that removes one of these three is worth what
/// that one costs and no more.
///
/// **These three are not the whole request.** A subscribe also runs the git
/// scan, the file-index walk, the reviews, the MCP read, the process walk and
/// the rest, so the sum here is a SHARE of a request rather than its cost -
/// and the share moves with the transcript and the machine, which is why the
/// printed line says so rather than quoting a percentage.
fn breakdown(surface: &Arc<ViewSurface>, seat: &SessionSlot, cwd: &Path) -> anyhow::Result<String> {
    let fleet = rss()?;

    let started = Instant::now();
    let read = surface.conversation(seat, cwd);
    let read_ms = started.elapsed().as_millis();
    let holding = rss()?;

    let started = Instant::now();
    let rendered = forge_server::transcript::render(&read.messages);
    let fold_ms = started.elapsed().as_millis();

    let started = Instant::now();
    let turns = forge_server::transport::wire::all_turns(&read.messages, &rendered);
    let encode_ms = started.elapsed().as_millis();

    Ok(format!(
        "breakdown: read {read_ms} ms, fold {fold_ms} ms, encode {encode_ms} ms over {} \
         messages (the CONVERSATION's share of a request, not the request: the git scan, the \
         file-index walk, the reviews, the MCP read and the process walk are not in these \
         three); resident {fleet} KiB with the fleet, {holding} KiB holding the read, {} KiB \
         holding read + fold + encode ({} turns)",
        read.messages.len(),
        rss()?,
        turns.len(),
    ))
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum;

    use super::Arm;

    /// An arm reports under the name it is ASKED FOR.
    ///
    /// **`name()` is a hand-written match and the report prints what it
    /// says**, so a variant renamed without it would run one workload and
    /// report another under the name a person typed - the defect this
    /// instrument exists to find, made by the instrument. The census is the
    /// enum's own, so a variant added is covered the day it lands.
    #[test]
    fn every_arm_reports_under_the_name_it_is_asked_for() {
        for arm in Arm::value_variants() {
            let asked =
                arm.to_possible_value().expect("every arm has a name").get_name().to_owned();
            assert_eq!(
                arm.name(),
                asked,
                "the name this arm reports under is not the one a caller asks for",
            );
        }
    }
}
