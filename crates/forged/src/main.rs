//! forged binary — daemon entrypoint.

#![allow(
    clippy::print_stdout,
    reason = "binary is allowed to print at top level"
)]

use clap::Parser;

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
    Listen,
    /// Show daemon status (connects to local daemon over loopback).
    Status,
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "M0 stub never errors; shape preserved for M1+"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Listen) {
        Cmd::Listen => {
            println!("forged listen — not yet implemented (M1)");
            Ok(())
        }
        Cmd::Status => {
            println!("forged status — not yet implemented (M1)");
            Ok(())
        }
    }
}
