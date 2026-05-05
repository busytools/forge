//! `forge-agent` — drives one [`forge_sdk::Client`] and exposes
//! callbacks + lifecycle to UI consumers.
//!
//! Created during the 2026-05-05 restructure (phase 3) — moved out of
//! `forge-tui::agent::*`. The crate evolves across the restructure:
//! phase 3 lifts the bridge body verbatim, phase 4 then pulls
//! userdata/cloud/env concerns in from forge-tui's app/ folder.
