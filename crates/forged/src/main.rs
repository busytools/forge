//! forged binary — daemon entrypoint.

#![allow(
    clippy::print_stdout,
    reason = "binary is allowed to print at top level"
)]

use clap::Parser;
use tokio::net::TcpListener;

use forged::registry::DaemonState;

#[derive(Parser, Debug)]
#[command(
    name = "forged",
    version,
    about = "forge daemon — JSON-RPC over WS wire to claude sessions"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Parser, Debug)]
enum Cmd {
    /// Start the daemon (default if no subcommand given).
    Listen {
        /// Address to bind. Default: 127.0.0.1:7373 (loopback only in M1).
        #[arg(default_value = "127.0.0.1:7373")]
        addr: String,
    },
    /// Show daemon status (connects to local daemon over loopback).
    Status,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Listen {
        addr: "127.0.0.1:7373".into(),
    }) {
        Cmd::Listen { addr } => {
            let listener = TcpListener::bind(&addr).await?;
            forged::server::run(listener, DaemonState::new()).await?;
            Ok(())
        }
        Cmd::Status => {
            forged::status_cli::run("127.0.0.1:7373").await?;
            Ok(())
        }
    }
}
