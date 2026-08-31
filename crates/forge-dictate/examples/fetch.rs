//! Fetch and verify every model a default `Config` names.
//!
//! Run:
//! ```bash
//! cargo run -p forge-dictate --release --example fetch
//! ```

use std::collections::HashMap;
use std::ops::ControlFlow;

use forge_dictate::{ConfigBuilder, Error, Progress};

fn main() -> Result<(), Error> {
    let cfg = ConfigBuilder::new().build();
    // Per file, because the models report interleaved: one shared
    // percent would let each model's progress suppress the other's.
    let mut shown: HashMap<String, u64> = HashMap::new();

    forge_dictate::prepare(&cfg, |progress| {
        match progress {
            Progress::Verifying { file } => println!("verifying {file}"),
            Progress::Downloading { file, downloaded, total } => {
                let percent = downloaded * 100 / total.max(1);
                if shown.insert(file.clone(), percent) != Some(percent) {
                    println!("{file} {percent}%");
                }
            }
            Progress::Ready { file } => println!("ready {file}"),
        }
        ControlFlow::Continue(())
    })
}
