//! `cargo test --doc` compiles the crate doc's opening example and nothing
//! else in this crate. A run over no fence at all exits 0 reporting
//! `running 0 tests`, and one over an `ignore` fence exits 0 reporting
//! `0 passed; N ignored`, so the gate cannot see its own subject's absence.
//! This fails instead.

#[test]
fn the_crate_doc_example_is_still_a_compiled_fence() {
    let src = include_str!("../src/lib.rs");
    let section = src.split_once("//! ## Minimal example").map_or("", |(_, rest)| rest);
    let fence = section.lines().find(|line| line.starts_with("//! ```")).unwrap_or("");

    assert!(
        !fence.contains("ignore"),
        "the crate doc's example is ignored, so `cargo test --doc` compiles nothing"
    );
    assert!(
        fence.contains("no_run"),
        "the crate doc's `## Minimal example` no longer opens a `no_run` fence, so `cargo test --doc` does not gate it"
    );
}
