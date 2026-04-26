//! forged binary — daemon entrypoint.

#![allow(
    clippy::print_stdout,
    reason = "binary is allowed to print at top level"
)]

use clap::Parser;
use tokio::net::TcpListener;

use forged::bind_check::is_loopback_bind;
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
        /// Address to bind. When omitted the daemon uses every entry in
        /// `config.bind` (default: `127.0.0.1:7373`). When provided this
        /// single address overrides the config — useful for ad-hoc one-port
        /// runs.
        addr: Option<String>,
    },
    /// Show daemon status (connects to local daemon over loopback).
    Status,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = forged::config::load_default()?;
    forged::logging::init(&config)?;

    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Listen { addr: None }) {
        Cmd::Listen { addr } => {
            let state = DaemonState::new();
            let binds: Vec<String> = match addr {
                Some(a) => vec![a],
                None if config.bind.is_empty() => vec!["127.0.0.1:7373".into()],
                None => config.bind.clone(),
            };

            // Warn loud and clear on non-loopback binds — forged has no
            // app-layer auth in this milestone; anyone with network
            // access to the listening port can drive `session.spawn`
            // and run arbitrary commands. Loopback / WireGuard mesh is
            // the only trust boundary today.
            for bind in &binds {
                if !is_loopback_bind(bind) {
                    tracing::warn!(
                        %bind,
                        "non-loopback bind without app-layer auth — anyone with network access can run arbitrary commands via session.spawn. WireGuard mesh is the only trust boundary."
                    );
                }
            }

            let mut handles = Vec::with_capacity(binds.len());
            for bind in binds {
                let listener = TcpListener::bind(&bind).await?;
                tracing::info!(%bind, "listening");
                let st = state.clone();
                handles.push(tokio::spawn(async move {
                    forged::server::run(listener, st).await
                }));
            }

            // Wait for any listener task to finish — typically forever, until SIGTERM.
            // The first task to exit (clean or otherwise) brings the daemon down.
            let (result, _idx, rest) = futures_util::future::select_all(handles).await;

            // Abort the remaining listener tasks and log any non-cancellation
            // errors so a second bind failure during shutdown isn't lost.
            for handle in rest {
                handle.abort();
                match handle.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "secondary listener exited with error during shutdown");
                    }
                    Err(join_err) if join_err.is_cancelled() => {}
                    Err(join_err) => {
                        tracing::warn!(error = %join_err, "secondary listener task panicked during shutdown");
                    }
                }
            }

            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(Box::new(e) as Box<dyn std::error::Error>),
                Err(join_err) => Err(Box::new(join_err) as Box<dyn std::error::Error>),
            }
        }
        Cmd::Status => {
            forged::status_cli::run("127.0.0.1:7373").await?;
            Ok(())
        }
    }
}
