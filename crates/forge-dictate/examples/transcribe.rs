//! Transcribe a 16 kHz mono WAV and print what was said.
//!
//! Run:
//! ```bash
//! cargo run -p forge-dictate --release --example transcribe -- clip.wav
//! ```

// Examples are illustrative; aborting on misuse is the right exit behaviour.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_dictate::{ConfigBuilder, Engine, Outcome, Samples};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: transcribe <clip.wav>");
    let bytes = std::fs::read(&path)?;
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let channels = u16::from_le_bytes(bytes[22..24].try_into().unwrap());
    let pcm: Vec<f32> = bytes[44..]
        .chunks_exact(2)
        .map(|p| f32::from(i16::from_le_bytes([p[0], p[1]])) / 32768.0)
        .collect();

    let engine = Engine::new(ConfigBuilder::new().build())?;
    match engine.transcribe(Samples::new(pcm, rate, channels))?.recv()? {
        Outcome::Transcript(t) => {
            println!("{}", t.text);
            if t.text != t.asr {
                println!("-- before normalization:\n{}", t.asr);
            }
            println!(
                "-- {:.2}s audio in {:.0}ms (mel {:.0} encode {:.0} decode {:.0})",
                t.stages.audio.as_secs_f64(),
                (t.stages.mel + t.stages.encode + t.stages.decode).as_secs_f64() * 1000.0,
                t.stages.mel.as_secs_f64() * 1000.0,
                t.stages.encode.as_secs_f64() * 1000.0,
                t.stages.decode.as_secs_f64() * 1000.0,
            );
            match t.stages.normalize {
                Some(d) => println!("-- normalize {:.0}ms", d.as_secs_f64() * 1000.0),
                None => println!("-- normalize: did not run"),
            }
        }
        Outcome::NoAudio { peak, audio } => {
            println!("no audio in {:.1}s (peak {peak:.1} dBFS)", audio.as_secs_f64());
        }
    }
    Ok(())
}
