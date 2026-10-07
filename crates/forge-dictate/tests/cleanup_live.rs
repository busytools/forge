//! The cleanup feed against the live Hub, for its numbers.
//!
//! `#[ignore]`d because it reaches the network. Run it to see what the
//! filters keep today:
//!
//!     cargo nextest run -p forge-dictate --test cleanup_live \
//!         --run-ignored all --no-capture
//!
//! It asserts nothing about the counts - the Hub's listing moves - and
//! prints them instead, the same way the normalizer baseline gate reports
//! rather than grades.

use forge_dictate::cleanup::{CleanupSource, fetch_cleanup};

#[test]
#[ignore = "reaches the Hub; run with --run-ignored all"]
fn the_live_cleanup_feed_and_what_our_filters_keep() {
    let entries = fetch_cleanup(&CleanupSource::default()).expect("the Hub answers");
    assert!(!entries.is_empty(), "the Hub answered, so the filters keep something");

    println!("\n{} candidates, in listing order:", entries.len());
    for entry in &entries {
        let quant = entry
            .downloads
            .iter()
            .map(|download| {
                format!(
                    "{} {} MB{}",
                    download.quant,
                    download.size_bytes / 1_000_000,
                    if download.sha256.is_some() { " +sha" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {:<56} {:<22} {}",
            entry.variant,
            format!("{:?}", entry.license.as_ref().map(|license| license.spdx.as_str())),
            quant
        );
    }
}
