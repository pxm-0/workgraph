//! The independent review: criterion verdicts and findings.

use super::{from_value, severity, Ctx};
use crate::ids::one_line;
use crate::paths::repo_path;
use serde::Deserialize;
use serde_json::Value;
use wg_factory::api::{CriterionVerdict, Finding, Review};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    passed: bool,
    criteria: Vec<WireVerdict>,
    findings: Vec<WireFinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireVerdict {
    id: String,
    status: String,
    #[serde(default)]
    evidence: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireFinding {
    pub(super) id: String,
    pub(super) severity: String,
    #[serde(default)]
    pub(super) file: Option<String>,
    pub(super) summary: String,
}

/// Criterion ids must be known; finding files must be repo paths.
pub fn parse(v: &Value, ctx: &Ctx) -> Result<Review, String> {
    let w: Wire = from_value(v, "the review")?;
    let criteria = w
        .criteria
        .into_iter()
        .map(|c| {
            if !ctx.known_criteria.contains(&c.id) {
                return Err(format!(
                    "the review names unknown criterion {:?}",
                    one_line(&c.id, 32)
                ));
            }
            let passed = match c.status.as_str() {
                "passed" => true,
                "failed" => false,
                _ => return Err(format!("criterion {} has an unknown status", c.id)),
            };
            Ok(CriterionVerdict {
                id: c.id,
                passed,
                evidence: c
                    .evidence
                    .iter()
                    .take(20)
                    .map(|e| one_line(e, 300))
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let findings = w
        .findings
        .into_iter()
        .map(finding)
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Review {
        passed: w.passed,
        criteria,
        findings,
    })
}

/// A finding's file must be a repo path; its text is one capped line.
pub(super) fn finding(f: WireFinding) -> Result<Finding, String> {
    let file = match f.file.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(p) => Some(repo_path(p)?),
    };
    Ok(Finding {
        id: one_line(&f.id, 32),
        severity: severity(&f.severity)?,
        file,
        summary: one_line(&f.summary, 1000),
        repro: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn review_needs_known_criteria_and_repo_paths() {
        let ctx = Ctx {
            known_criteria: vec!["AC-1".into()],
            ..Ctx::default()
        };
        let ok = json!({ "passed": false, "criteria": [{ "id": "AC-1", "status": "failed" }],
                         "findings": [{ "id": "RF-1", "severity": "high", "file": "src/a.js", "summary": "s" },
                                      { "id": "RF-2", "severity": "low", "file": "", "summary": "s" }] });
        let r = parse(&ok, &ctx).unwrap();
        assert!(!r.criteria[0].passed && r.findings[1].file.is_none());
        for bad in [
            json!({ "passed": true, "criteria": [{ "id": "AC-9", "status": "passed" }], "findings": [] }),
            json!({ "passed": true, "criteria": [{ "id": "AC-1", "status": "maybe" }], "findings": [] }),
            json!({ "passed": true, "criteria": [], "findings": [{ "id": "R", "severity": "high", "file": "/etc/x", "summary": "s" }] }),
            json!({ "passed": true, "criteria": [], "findings": [{ "id": "R", "severity": "huge", "summary": "s" }] }),
        ] {
            assert!(parse(&bad, &ctx).is_err(), "{bad}");
        }
    }
}
