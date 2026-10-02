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

    // Token-wise rather than substring: rustdoc reads the first token as a
    // language and drops the whole info string when it is not Rust, so
    // `rs,no_run` compiles nothing and a `contains` check calls that gated.
    let tokens: Vec<&str> =
        fence.strip_prefix("//! ```").unwrap_or("").split(',').map(str::trim).collect();
    let language = tokens.first().copied().unwrap_or("");

    assert!(
        matches!(language, "" | "rust" | "no_run")
            && tokens.contains(&"no_run")
            && !tokens.contains(&"ignore"),
        "the crate doc's `## Minimal example` is not a compiled, non-running Rust fence, so `cargo test --doc` does not gate it"
    );
}
