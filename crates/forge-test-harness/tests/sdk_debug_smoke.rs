//! Minimal diagnostic harness — bypasses `Client::spawn` to observe the
//! raw wire conversation between forge-sdk and a live `claude`. Dumps
//! every line as it arrives (even if the test later hangs) so we can
//! tell exactly where the stall happens.


use std::io::Write as _;
use std::time::{Duration, Instant};

use forge_sdk::OptionsBuilder;
use forge_sdk::transport::process::Subprocess;

#[tokio::test]
#[ignore = "diagnostic; opt-in via FORGE_WIRE_DEBUG=1"]
async fn wire_debug_trivial() {
    if std::env::var("FORGE_WIRE_DEBUG").is_err() {
        return;
    }

    let dump_path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wire-debug.log");
    let mut dump = std::fs::File::create(&dump_path).expect("open dump");
    let start = Instant::now();
    let log = |dump: &mut std::fs::File, msg: &str| {
        let elapsed = start.elapsed().as_millis();
        writeln!(dump, "+{elapsed}ms {msg}").ok();
        dump.flush().ok();
        eprintln!("+{elapsed}ms {msg}");
    };

    let opts = OptionsBuilder::new().max_turns(1).build();
    let mut sub = Subprocess::spawn(&opts).await.expect("spawn");
    log(&mut dump, "subprocess spawned");

    // Write initialize FIRST — the CLI gates system/init on receiving
    // the initialize control_request.
    let init_req = "{\"type\":\"control_request\",\"request_id\":\"dbg-1\",\
         \"request\":{\"subtype\":\"initialize\",\"hooks\":null}}\n";
    sub.write_line(init_req).await.expect("write initialize");
    log(&mut dump, "initialize sent");
    // Send a user message so CLI has reason to emit system/init + start
    // the conversation. Testing whether the CLI withholds init until it
    // sees user input.
    let user_msg = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"ping\"}}\n";
    sub.write_line(user_msg).await.expect("write user");
    log(&mut dump, "user message sent");

    // Read frames for up to 30s, looking for control_response + init.
    let deadline_read1 = Duration::from_secs(30);
    let t0 = Instant::now();
    let mut frames = 0;
    while t0.elapsed() < deadline_read1 && frames < 20 {
        match tokio::time::timeout(Duration::from_secs(5), sub.read_line()).await {
            Ok(Ok(Some(line))) => {
                frames += 1;
                let trimmed = line.trim_end();
                let preview = trimmed.chars().take(120).collect::<String>();
                log(&mut dump, &format!("[in #{frames}] {preview}"));
            }
            Ok(Ok(None)) => {
                log(&mut dump, "EOF on stdout");
                break;
            }
            Ok(Err(e)) => {
                log(&mut dump, &format!("read error: {e}"));
                break;
            }
            Err(_timeout) => {
                log(&mut dump, "read_line timed out after 5s — inspecting");
            }
        }
    }
    let _ = sub.close().await;
    log(&mut dump, "done");
    eprintln!("debug log: {}", dump_path.display());
}
