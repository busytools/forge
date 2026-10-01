//! The protocol client: the instrument that speaks the socket from the other
//! side of it.
//!
//! It is what `curl` cannot be for a socket. It connects to a running forge,
//! sends a script of messages as a client would, and prints everything the
//! server says back as one line of JSON each - so a person debugging a client
//! has both halves of the conversation in front of them rather than a guess
//! about which end is wrong.
//!
//! ```text
//! forge-protocol-client --url ws://127.0.0.1:8790/socket --script ./walk.txt
//! ```
//!
//! A script is the protocol in miniature rather than a vocabulary of its own:
//! a verb and its arguments, and raw JSON where a message carries a payload.
//!
//! ```text
//! # a comment
//! subscribe home
//! subscribe session Busytools/forge/lead
//! more Busytools/forge/lead 10
//! command {"prompt": {"key": {"org": "Busytools", "project": "forge", "label": "lead"}, "text": "hello", "attachments": []}}
//! ask {"submit_review": {"project": "forge", "branch": "main", "summary": null, "thread_ids": [], "origin": {"org": "Busytools", "project": "forge", "label": "lead"}}}
//! ```
//!
//! `ask` is `command` with a reply asked for, so the awaited `Reply` path is
//! exercised rather than only the fire-and-forget one.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use forge_primitives::SessionSlot;
use forge_server::Command;
use forge_server::transport::envelope::{ClientMessage, Subject};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
struct Args {
    /// The socket to speak to, e.g. `ws://127.0.0.1:8790/socket`.
    #[arg(long)]
    url: String,
    /// A file of script lines, one per line; blank lines and `#` are skipped.
    #[arg(long)]
    script: PathBuf,
    /// How long to keep reading after the script has been sent. The client
    /// exits at this bound whether or not the server has stopped talking, so
    /// a run against a quiet server ends rather than hanging.
    #[arg(long, default_value_t = 10)]
    seconds: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let script = std::fs::read_to_string(&args.script)?;
    let lines: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();

    let (mut socket, _) = tokio_tungstenite::connect_async(&args.url).await?;
    // The greeting first, so a reader sees the server speak before it is
    // asked anything - and sees WHAT answered, which is the whole reason the
    // greeting exists.
    if let Some(Ok(Message::Text(greeting))) = next_within(&mut socket, args.seconds).await {
        println!("{greeting}");
    }

    let mut reply_to = 1_u64;
    for line in lines {
        let message = parse_line(line, &mut reply_to)?;
        let text = serde_json::to_string(&message)?;
        socket.send(Message::Text(text.into())).await?;
    }

    // One deadline for the whole run rather than one per read, so a chatty
    // server cannot keep this alive past the bound the caller asked for.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.seconds);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(Ok(msg)) = next_within(&mut socket, left.as_secs()).await else {
            break;
        };
        match msg {
            Message::Text(text) => println!("{text}"),
            Message::Close(_) => break,
            // A ping or a pong carries nothing to say, and the socket is
            // still open.
            _ => {}
        }
    }
    Ok(())
}

/// The socket's next message, or `None` once it stops arriving.
async fn next_within(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    seconds: u64,
) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    tokio::time::timeout(Duration::from_secs(seconds), socket.next()).await.ok().flatten()
}

/// One script line as a message to send.
///
/// The lines name what the protocol has rather than inventing a second
/// vocabulary: a verb and its subject, and raw JSON where a message carries a
/// payload, so the instrument sends what a client would send.
fn parse_line(line: &str, reply_to: &mut u64) -> anyhow::Result<ClientMessage> {
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    match verb {
        // This instrument drives the socket the way a view does, so it
        // declares itself able to answer: a client that cannot show a prompt
        // must not be counted as one, but this one walks the reply path.
        "subscribe" => Ok(ClientMessage::Subscribe { what: subject(rest)?, answering: true }),
        "unsubscribe" => Ok(ClientMessage::Unsubscribe { what: subject(rest)? }),
        "more" => {
            let mut parts = rest.split_whitespace();
            let Some(seat) = parts.next() else {
                anyhow::bail!("`more` wants a session subject");
            };
            let Subject::Session(conversation) = subject(&format!("session {seat}"))? else {
                anyhow::bail!("`more` wants a session subject");
            };
            let turns = parts.next().map_or(Ok(10), str::parse)?;
            Ok(ClientMessage::More { conversation, before: None, turns })
        }
        "devices" => Ok(ClientMessage::Devices),
        "command" | "ask" => {
            let command: Command = serde_json::from_str(rest)?;
            let asked = if verb == "ask" {
                let asked = Some(*reply_to);
                *reply_to += 1;
                asked
            } else {
                None
            };
            Ok(ClientMessage::Command { command: Box::new(command), reply_to: asked })
        }
        other => anyhow::bail!("`{other}` is not a verb this client knows"),
    }
}

/// `home`, `usage`, or `session <org>/<project>/<label>`.
fn subject(rest: &str) -> anyhow::Result<Subject> {
    if rest == "home" {
        return Ok(Subject::Home);
    }
    if rest == "usage" {
        return Ok(Subject::Usage);
    }
    let Some(seat) = rest.strip_prefix("session ") else {
        anyhow::bail!(
            "a subject is `home`, `usage` or `session <org>/<project>/<label>`, not `{rest}`"
        );
    };
    let mut parts = seat.split('/');
    let (Some(org), Some(project), Some(label), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        anyhow::bail!("a session subject is <org>/<project>/<label>, not `{seat}`");
    };
    Ok(Subject::Session(SessionSlot::for_label(org, project, Some(label))))
}
