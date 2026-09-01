//! Record from the default microphone for a few seconds, then print what
//! was said.
//!
//! Run:
//! ```bash
//! cargo run -p forge-dictate --release --example dictate -- 5
//! ```

// Examples are illustrative; aborting on misuse is the right exit behaviour.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use forge_dictate::{ConfigBuilder, Engine, Outcome};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = std::env::args().nth(1).unwrap_or_else(|| "5".into()).parse()?;

    let engine = Engine::new(ConfigBuilder::new().build())?;
    let capture = engine.try_capture("example")?;
    println!("recording for {seconds}s...");
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        // `level` is take-and-reset: each read is the peak over the
        // window since the previous one, not a running high-water mark.
        println!("  peak since last read {:.1} dBFS", capture.level());
    }

    match capture.finish()?.recv()? {
        Outcome::Transcript(t) => {
            println!("\n{}", t.text);
            if t.text != t.asr {
                println!("\n-- before normalization:\n{}", t.asr);
            }
            if t.truncated {
                println!("(cut short at the capture cap)");
            }
        }
        Outcome::NoAudio { peak, audio } => {
            println!("\nno audio in {:.1}s (peak {peak:.1} dBFS)", audio.as_secs_f64());
        }
    }
    Ok(())
}
