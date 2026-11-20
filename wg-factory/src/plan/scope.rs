//! Scope: the user's uncommitted work, harness files and what the run changed outside its plan.

use crate::model::Unit;
use alloc::string::String;
use alloc::vec::Vec;

pub const LEGACY_MEMORY: &str = ".claude/work-memory.json";

/// Non-risky planned files that hold the user's uncommitted work.
pub fn dirty_overlap(units: &[Unit], user_changes: &[String]) -> Vec<String> {
    units
        .iter()
        .filter(|u| !u.risky)
        .flat_map(|u| u.files.iter())
        .filter(|f| user_changes.contains(f))
        .cloned()
        .collect()
}

/// The user's changes that stay out of the run's commit.
pub fn committable(
    user_changes: &[String],
    overlap: &[String],
    dirty_decided: bool,
) -> Vec<String> {
    if dirty_decided {
        user_changes
            .iter()
            .filter(|f| !overlap.contains(f))
            .cloned()
            .collect()
    } else {
        user_changes.to_vec()
    }
}

/// Changed files that no unit planned, other than the user's work, files changed before the run,
/// the legacy memory file and Haruspex harness files.
pub fn out_of_scope(
    changed: &[String],
    units: &[Unit],
    user_changes: &[String],
    preexisting: &[String],
    initialized: bool,
) -> Vec<String> {
    let planned: Vec<&String> = units.iter().flat_map(|u| u.files.iter()).collect();
    let in_plan = |f: &String| {
        planned
            .iter()
            .any(|p| *p == f || (f.ends_with('/') && p.starts_with(f.as_str())))
    };
    let harness = |f: &String| {
        f.starts_with(".haruspex/") || (initialized && (f == "AGENTS.md" || f == "CLAUDE.md"))
    };
    changed
        .iter()
        .filter(|f| {
            !user_changes.contains(f)
                && !preexisting.contains(f)
                && f.as_str() != LEGACY_MEMORY
                && !harness(f)
                && !in_plan(f)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Size, Tier};
    use alloc::string::ToString;
    use alloc::vec;

    fn u(id: &str, files: &[&str], risky: bool) -> Unit {
        Unit {
            id: id.to_string(),
            goal: String::new(),
            files: files.iter().map(|f| f.to_string()).collect(),
            depends_on: vec![],
            criteria: vec![],
            size: Size::Small,
            risky,
            risk_reason: None,
            tier: Tier::Haiku,
            counts_for_tier: true,
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn overlap_ignores_risky_units_and_committable_drops_it_only_on_a_decision() {
        let units = [u("a", &["src/a.js"], false), u("r", &["db.sql"], true)];
        let changes = s(&["src/a.js", "db.sql", "notes.md"]);
        let overlap = dirty_overlap(&units, &changes);
        assert_eq!(overlap, s(&["src/a.js"]));
        assert_eq!(committable(&changes, &overlap, false), changes);
        assert_eq!(
            committable(&changes, &overlap, true),
            s(&["db.sql", "notes.md"])
        );
    }

    #[test]
    fn out_of_scope_skips_harness_user_and_planned_files() {
        let units = [u("a", &["src/a.js", "docs/x.md"], false)];
        let changed = s(&[
            "src/a.js",
            "docs/",
            "lib/",
            "wip.md",
            "old.txt",
            ".haruspex/state.json",
            "AGENTS.md",
            ".claude/work-memory.json",
            "stray.txt",
        ]);
        let got = out_of_scope(&changed, &units, &s(&["wip.md"]), &s(&["old.txt"]), true);
        assert_eq!(got, s(&["lib/", "stray.txt"]));
        let not_init = out_of_scope(&s(&["CLAUDE.md"]), &units, &[], &[], false);
        assert_eq!(not_init, s(&["CLAUDE.md"]));
    }
}
