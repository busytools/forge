//! forge-tui binary — terminal client entrypoint.

#![allow(
    clippy::print_stdout,
    reason = "binary is allowed to print at top level"
)]

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "forge-tui", version, about = "forge terminal client")]
struct Cli {
    /// forged hostname:port (e.g. forged.example.com or 10.x.x.x:7373).
    #[arg(long, default_value = "127.0.0.1:7373")]
    forged: String,
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "M0 stub never errors; shape preserved for M7"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _cli = Cli::parse();
    println!("forge-tui — not yet implemented (M7)");
    Ok(())
}
