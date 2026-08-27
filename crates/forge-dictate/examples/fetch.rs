//! Fetch and verify every model a default `Config` names.
//!
//! Run:
//! ```bash
//! cargo run -p forge-dictate --release --example fetch
//! ```

// Examples are illustrative; aborting on misuse is the right exit behaviour.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::ops::ControlFlow;

use forge_dictate::{ConfigBuilder, Error, Progress};

fn main() -> Result<(), Error> {
    let cfg = ConfigBuilder::new().build();
    let mut shown = u64::MAX;

    forge_dictate::prepare(&cfg, |progress| {
        match progress {
            Progress::Verifying { file } => println!("verifying {file}"),
            Progress::Downloading { file, downloaded, total } => {
                let percent = downloaded * 100 / total.max(1);
                if percent != shown {
                    shown = percent;
                    println!("{file} {percent}%");
                }
            }
            Progress::Ready { file } => println!("ready {file}"),
        }
        ControlFlow::Continue(())
    })
}
