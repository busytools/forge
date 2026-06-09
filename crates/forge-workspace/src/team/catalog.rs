//! Role catalog: which forge-team roles a project may spawn, and the
//! delegation-catalog text injected into Lead sessions' system prompt.
//!
//! Global roles live at `~/.claude/forge-team/<role>/charter.md`
//! (available to every project). Project roles live at
//! `~/.claude/forge-team/<project>/<role>/charter.md` (scoped to that
//! project). `lead` is reserved framing, never a spawnable delegate.

use std::path::Path;

use crate::team::roles::{LEAD_LABEL, forge_team_root};

/// One spawnable role and its one-line catalog description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleSummary {
    pub label: String,
    pub description: String,
}

/// First non-empty line of a charter, used as its catalog blurb. A
/// leading `description:` wins; otherwise the line with markdown
/// heading / quote markers stripped. Truncated to 100 chars.
pub fn description_from_charter(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let body = match first.strip_prefix("description:") {
        Some(rest) => rest.trim(),
        None => first.trim_start_matches(['#', '>', ' ']),
    };
    truncate(body, 100)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

/// Roles a session in `namespace` may spawn: top-level globals
/// (excluding `lead`) plus that project's `<namespace>/*` roles. Reads
/// the live forge-team dir; any IO failure degrades to fewer entries
/// rather than erroring (a missing catalog must never block a spawn).
/// Ordered globals-first, alphabetical within each group.
pub fn scan_catalog(namespace: Option<&str>) -> Vec<RoleSummary> {
    let Some(root) = forge_team_root() else {
        return Vec::new();
    };
    let mut globals = collect_roles(&root, None);
    let mut project = match namespace {
        Some(ns) => collect_roles(&root.join(ns), Some(ns)),
        None => Vec::new(),
    };
    globals.sort_by(|a, b| a.label.cmp(&b.label));
    project.sort_by(|a, b| a.label.cmp(&b.label));
    globals.extend(project);
    globals
}

/// Collect `<dir>/<role>/charter.md` entries. When `namespace` is
/// `Some(ns)` the produced labels are `ns/<role>`; when `None` they are
/// bare global labels (with `lead` filtered out). A subdir without a
/// direct `charter.md` (e.g. a project-namespace dir while scanning the
/// root) is skipped, which is how globals are distinguished from
/// namespaces.
fn collect_roles(dir: &Path, namespace: Option<&str>) -> Vec<RoleSummary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(role) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if namespace.is_none() && role == LEAD_LABEL {
            continue;
        }
        let charter_path = entry.path().join("charter.md");
        let Ok(text) = std::fs::read_to_string(&charter_path) else {
            continue; // no direct charter.md => namespace dir or empty; skip
        };
        let label = match namespace {
            Some(ns) => format!("{ns}/{role}"),
            None => role,
        };
        out.push(RoleSummary { label, description: description_from_charter(&text) });
    }
    out
}

/// Render the delegation-catalog system-prompt section for a Lead
/// session. Always emits the capability preamble; appends the role
/// list when non-empty.
pub fn render_catalog(roles: &[RoleSummary]) -> String {
    let mut s = String::from(
        "You can delegate work to peer worker sessions via the \
         mcp__forge__workers__ tools. Spawn one with \
         workers__spawn(label=\"<role>\") and its charter loads \
         automatically; talk to it with workers__tell / workers__ask; \
         list live workers with workers__list. At most one live worker \
         exists per label - if it already exists, message it instead of \
         spawning again. Default to doing the work yourself; delegate \
         only substantial or parallelizable work.",
    );
    if !roles.is_empty() {
        s.push_str("\n\nRoles you can spawn here:");
        for r in roles {
            s.push_str("\n- ");
            s.push_str(&r.label);
            if !r.description.is_empty() {
                s.push_str(" - ");
                s.push_str(&r.description);
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::roles::set_forge_team_root_for_test;
    use std::fs;

    #[test]
    fn description_prefers_explicit_line_then_heading() {
        assert_eq!(
            description_from_charter("description:  Generic code-writer\n# x"),
            "Generic code-writer"
        );
        assert_eq!(description_from_charter("# Forge Implementer\nbody"), "Forge Implementer");
        assert_eq!(description_from_charter("\n\n> quoted first"), "quoted first");
    }

    #[test]
    fn scan_lists_globals_plus_own_project_excludes_others() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // globals
        write_charter(root, "implementer", "description: Generic code-writer\n");
        write_charter(root, "lead", "description: lead\n"); // excluded
        // project namespaces
        write_charter(root, "hub-modules/steward", "description: Hub steward\n");
        write_charter(root, "forge/probe", "description: Forge probe\n");
        let prev = set_forge_team_root_for_test(Some(root.to_path_buf()));

        let forge = scan_catalog(Some("forge"));
        let labels: Vec<_> = forge.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"implementer"));
        assert!(labels.contains(&"forge/probe"));
        assert!(!labels.contains(&"hub-modules/steward")); // other project hidden
        assert!(!labels.contains(&"lead")); // reserved

        set_forge_team_root_for_test(prev);
    }

    fn write_charter(root: &std::path::Path, label: &str, body: &str) {
        let dir = root.join(label);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("charter.md"), body).expect("write");
    }
}
