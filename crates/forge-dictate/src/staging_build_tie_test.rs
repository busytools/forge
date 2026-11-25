//! The build script must compile the staging gate's own source.
//!
//! The staging test drives `crate::staging`, a second compilation of the
//! same file `build.rs` includes. An inlined copy in `build.rs` could go
//! back to rewriting files in place while every test stayed green, so the
//! include is asserted rather than left to review.

#[test]
fn the_build_script_stages_through_the_shared_file() {
    let build = include_str!("../build.rs");
    assert!(
        build.contains("include!(\"src/staging.rs\")"),
        "build.rs must compile src/staging.rs; the staging gate only proves what that file does"
    );
    assert!(
        !build.contains("fn stage("),
        "build.rs must not carry a second staging implementation the gate cannot see"
    );
}
