//! Which units a failure goes back to.

use crate::api::{CheckOutcome, Review};
use crate::model::Unit;
use alloc::vec::Vec;

/// shortcut: blames units whose paths (or basenames) appear in the failure output, else the whole
/// layer; add a triage agent if blame is often wrong.
pub fn blame(failed: &[CheckOutcome], units: &[Unit]) -> Vec<Unit> {
    let text: Vec<&str> = failed.iter().map(|f| f.output_tail.as_str()).collect();
    let mentions = |needle: &str| text.iter().any(|t| t.contains(needle));
    let hit: Vec<Unit> = units
        .iter()
        .filter(|u| {
            u.files
                .iter()
                .any(|f| mentions(f) || mentions(f.rsplit('/').next().unwrap_or(f)))
        })
        .cloned()
        .collect();
    if hit.is_empty() {
        units.to_vec()
    } else {
        hit
    }
}

/// Serious findings name a file, failed criteria name a unit; anything unplaced goes to every unit.
pub fn blame_review(review: &Review, units: &[Unit]) -> Vec<Unit> {
    let files: Vec<&str> = review
        .findings
        .iter()
        .filter(|f| f.severity.serious())
        .filter_map(|f| f.file.as_deref())
        .collect();
    let failed: Vec<&str> = review
        .criteria
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.id.as_str())
        .collect();
    let hit: Vec<Unit> = units
        .iter()
        .filter(|u| {
            u.files.iter().any(|f| files.contains(&f.as_str()))
                || u.criteria.iter().any(|c| failed.contains(&c.as_str()))
        })
        .cloned()
        .collect();
    if hit.is_empty() {
        units.to_vec()
    } else {
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CriterionVerdict, Finding, Severity, Size, Tier};
    use alloc::string::{String, ToString};
    use alloc::vec;

    fn u(id: &str, file: &str, crit: &str) -> Unit {
        Unit {
            id: id.to_string(),
            goal: String::new(),
            files: vec![file.to_string()],
            depends_on: vec![],
            criteria: vec![crit.to_string()],
            size: Size::Small,
            risky: false,
            risk_reason: None,
            tier: Tier::Haiku,
            counts_for_tier: true,
        }
    }

    fn out(tail: &str) -> CheckOutcome {
        CheckOutcome {
            name: "test".to_string(),
            cmd: "npm test".to_string(),
            exit_code: 1,
            output_tail: tail.to_string(),
        }
    }

    fn ids(v: &[Unit]) -> Vec<&str> {
        v.iter().map(|u| u.id.as_str()).collect()
    }

    #[test]
    fn blame_by_path_or_basename_else_the_whole_layer() {
        let units = [u("a", "src/a.js", "AC-a"), u("b", "lib/deep/b.js", "AC-b")];
        assert_eq!(ids(&blame(&[out("FAIL src/a.js:3")], &units)), ["a"]);
        assert_eq!(ids(&blame(&[out("at b.js line 9")], &units)), ["b"]);
        assert_eq!(ids(&blame(&[out("boom")], &units)), ["a", "b"]);
    }

    #[test]
    fn review_blame_uses_serious_finding_files_and_failed_criteria() {
        let units = [u("a", "src/a.js", "AC-a"), u("b", "src/b.js", "AC-b")];
        let finding = |sev, file: &str| Finding {
            id: "RF-1".to_string(),
            severity: sev,
            file: Some(file.to_string()),
            summary: String::new(),
            repro: None,
        };
        let review = Review {
            passed: false,
            criteria: vec![],
            findings: vec![
                finding(Severity::Low, "src/a.js"),
                finding(Severity::High, "src/b.js"),
            ],
        };
        assert_eq!(ids(&blame_review(&review, &units)), ["b"]);
        let by_criterion = Review {
            passed: false,
            criteria: vec![CriterionVerdict {
                id: "AC-a".to_string(),
                passed: false,
                evidence: vec![],
            }],
            findings: vec![],
        };
        assert_eq!(ids(&blame_review(&by_criterion, &units)), ["a"]);
        let nothing = Review {
            passed: false,
            criteria: vec![],
            findings: vec![],
        };
        assert_eq!(ids(&blame_review(&nothing, &units)), ["a", "b"]);
    }
}
