//! The marks forge ships, by the name `[web] mark` takes.
//!
//! Every mark is one `<svg viewBox="0 0 24 24">` drawn in `currentColor`,
//! so switching between the accent and a single colour is a `color` change
//! on the caller and nothing else.

/// The mark drawn when no name is set: a solid tile with an arched kiln
/// door knocked out of the bottom.
pub const DEFAULT_MARK: &str = "klin";

const KLIN: &str = r#"<path fill="currentColor" fill-rule="evenodd" d="M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Zm2.5 19v-8.5a4.5 4.5 0 0 1 9 0V22h-9Z"/>"#;

/// The shapes inside a `<svg viewBox="0 0 24 24">`, in `currentColor`
/// throughout so the caller's `color` decides what the mark is drawn in.
pub fn mark_path(name: Option<&str>) -> &'static str {
    match name {
        Some("lanes") => {
            r#"<g fill="currentColor"><rect x="2.5" y="8.5" width="4" height="12.5" rx="2"/><rect x="10" y="3" width="4" height="18" rx="2"/><rect x="17.5" y="12" width="4" height="9" rx="2"/></g>"#
        }
        Some("split") => {
            r#"<g fill="currentColor"><rect x="9" y="2.5" width="6" height="4.5" rx="2"/><rect x="10.6" y="6.4" width="2.8" height="3"/><rect x="3" y="8.8" width="18" height="3" rx="1.5"/><rect x="4.25" y="11.8" width="3" height="9.7" rx="1.5"/><rect x="10.5" y="11.8" width="3" height="9.7" rx="1.5"/><rect x="16.75" y="11.8" width="3" height="9.7" rx="1.5"/></g>"#
        }
        Some("spine") => {
            r#"<g fill="currentColor"><rect x="4" y="2.5" width="3.5" height="19" rx="1.75"/><rect x="7.5" y="6.6" width="7" height="3" rx="1"/><rect x="14.5" y="5.1" width="6" height="6" rx="1.8"/><rect x="7.5" y="14.4" width="7" height="3" rx="1"/><rect x="14.5" y="12.9" width="6" height="6" rx="1.8"/></g>"#
        }
        Some("slab") => {
            r#"<g fill="currentColor"><path d="M12 3.2 21 5.8 12 8.4 3 5.8Z"/><path d="M12 10.2 21 12.8 12 15.4 3 12.8Z"/><path d="M12 17.2 21 19.8 12 22.4 3 19.8Z"/></g>"#
        }
        Some("grid") => {
            r#"<g fill="none" stroke="currentColor" stroke-width="2.2"><rect x="4.1" y="4.1" width="5.3" height="5.3" rx="1.5"/><rect x="14.6" y="4.1" width="5.3" height="5.3" rx="1.5"/><rect x="14.6" y="14.6" width="5.3" height="5.3" rx="1.5"/></g><rect x="4.1" y="14.6" width="5.3" height="5.3" rx="1.5" fill="currentColor"/>"#
        }
        Some("clamp") => {
            r#"<g fill="currentColor"><rect x="2.5" y="3" width="19" height="4" rx="2"/><rect x="6.5" y="9.5" width="11" height="5" rx="1.6"/><rect x="2.5" y="17" width="19" height="4" rx="2"/></g>"#
        }
        Some("strike") => {
            r#"<g fill="currentColor"><rect x="9.9" y="1" width="4.2" height="22" rx="2.1" transform="rotate(45 12 12)"/><rect x="9.9" y="1" width="4.2" height="22" rx="2.1" transform="rotate(-45 12 12)"/></g>"#
        }
        Some("cascade") => {
            r#"<g fill="currentColor"><path d="M3.4 .6H20.6V3.8L12 7.4 3.4 3.8Z"/><path d="M3.4 8.6H20.6V11.8L12 15.4 3.4 11.8Z"/><path d="M3.4 16.6H20.6V19.8L12 23.4 3.4 19.8Z"/></g>"#
        }
        Some("nest") => {
            r#"<g fill="none" stroke="currentColor" stroke-width="2.2"><rect x="3.9" y="3.9" width="16.2" height="16.2" rx="4"/><rect x="9" y="9" width="6" height="6" rx="1.8"/></g>"#
        }
        Some("chamfer") => {
            r#"<path fill="currentColor" d="M7 3h9l5 5v9a4 4 0 0 1-4 4H7a4 4 0 0 1-4-4V7a4 4 0 0 1 4-4Z"/>"#
        }
        Some("tally") => {
            r#"<g fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round"><path d="M3.5 6.5V17.5M8.5 5V19M13.5 5V19M18.5 6.5V17.5M2.4 17.2 21.6 6.8"/></g>"#
        }
        Some("stencil_f") => {
            r#"<path fill="currentColor" fill-rule="evenodd" d="M6.5 3h11a3.5 3.5 0 0 1 3.5 3.5v11a3.5 3.5 0 0 1-3.5 3.5h-11a3.5 3.5 0 0 1-3.5-3.5v-11a3.5 3.5 0 0 1 3.5-3.5Zm1 3.5h3.5v11h-3.5Zm3.5 0h7v3.5h-7Zm0 5h5v3.5h-5Z"/>"#
        }
        Some("f_slab") => {
            r#"<g fill="currentColor"><rect x="4" y="3.5" width="16" height="4.5" rx="1.4"/><rect x="4" y="10.5" width="11" height="4.5" rx="1.4"/><rect x="4" y="3.5" width="4.5" height="17" rx="1.4"/></g>"#
        }
        Some("anvil") => {
            r#"<path fill="currentColor" fill-rule="evenodd" d="M6 2.5h12a3.5 3.5 0 0 1 3.5 3.5v12a3.5 3.5 0 0 1-3.5 3.5H6A3.5 3.5 0 0 1 2.5 18V6A3.5 3.5 0 0 1 6 2.5Zm-.5 4.5v3.4h13V7Zm4 3.4v3.6h5v-3.6Zm-3 3.6v3.4h11V14Z"/>"#
        }
        Some("spark") => {
            r#"<path fill="currentColor" d="M12 1.5Q13.6 10.4 22.5 12 13.6 13.6 12 22.5 10.4 13.6 1.5 12 10.4 10.4 12 1.5Z"/>"#
        }
        // `klin`, an unset name, and any name outside the list all draw the
        // built-in: the loader refuses the last at boot, so this is the
        // renderer's backstop rather than a fallback path.
        _ => KLIN,
    }
}

/// The mark as a standalone `<svg>` element. Decorative wherever it is
/// drawn - the wordmark beside it carries the name - so assistive tech is
/// told to skip it.
pub fn mark_svg(name: Option<&str>) -> String {
    format!(r#"<svg viewBox="0 0 24 24" aria-hidden="true">{}</svg>"#, mark_path(name))
}

#[cfg(test)]
mod tests {
    use forge_primitives::web::MARK_NAMES;

    use super::{DEFAULT_MARK, mark_path};

    /// Every name `forge.toml` accepts has a drawing of its own, so a name
    /// added to the shipped list without a mark cannot quietly render as
    /// the built-in one.
    #[test]
    fn every_shipped_name_has_its_own_drawing() {
        let built_in = mark_path(None);
        for name in MARK_NAMES.iter().filter(|name| **name != DEFAULT_MARK) {
            assert_ne!(
                mark_path(Some(name)),
                built_in,
                "{name} is a name forge.toml accepts but draws the built-in mark",
            );
        }
    }

    /// An unset name and the built-in's own name draw the same mark, and
    /// the built-in is one the shipped list offers.
    #[test]
    fn the_built_in_mark_is_an_unset_name() {
        assert_eq!(mark_path(None), mark_path(Some(DEFAULT_MARK)));
        assert!(
            MARK_NAMES.contains(&DEFAULT_MARK),
            "the built-in has to be a name forge.toml accepts, or nothing could name it",
        );
    }
}
