fn main() {
    // **The engine is the vendored Chromium again** (CEF was measured out
    // 2026-10-07: its windowed runtime drops agent input in the background
    // and wedges the pinned driver's click - see `chromium::show` for the
    // hand-off's own window). The CEF bootstrap and its module stay for a
    // later engine, dormant.
    forge_client::run();
}
