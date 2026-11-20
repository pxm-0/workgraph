//! Replies of the Phase 4 agent stages: a QA explorer's verdict, release notes and the release
//! writer's word on its plan (the plan itself is in the Haruspex task, checked by the gate).

use super::from_value;
use super::review::{finding, WireFinding};
use crate::ids::one_line;
use serde::Deserialize;
use serde_json::Value;
use wg_factory::api::{ReleaseNotes, ReleasePlan, Verdict};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireVerdict {
    passed: bool,
    findings: Vec<WireFinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNotes {
    summary: String,
    changes: Vec<String>,
    breaking: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePlan {
    ready: bool,
    summary: String,
}

/// At most 50 findings, each file a repo path.
pub fn verdict(v: &Value) -> Result<Verdict, String> {
    let w: WireVerdict = from_value(v, "the verdict")?;
    if w.findings.len() > 50 {
        return Err("the verdict has more than 50 findings".into());
    }
    Ok(Verdict {
        passed: w.passed,
        findings: w
            .findings
            .into_iter()
            .map(finding)
            .collect::<Result<_, _>>()?,
    })
}

pub fn notes(v: &Value) -> Result<ReleaseNotes, String> {
    let w: WireNotes = from_value(v, "the release notes")?;
    if w.changes.len() > 50 {
        return Err("the release notes list more than 50 changes".into());
    }
    Ok(ReleaseNotes {
        summary: one_line(&w.summary, 300),
        changes: w.changes.iter().map(|c| one_line(c, 300)).collect(),
        breaking: w.breaking,
    })
}

pub fn plan(v: &Value) -> Result<ReleasePlan, String> {
    let w: WirePlan = from_value(v, "the release plan")?;
    Ok(ReleasePlan {
        ready: w.ready,
        summary: one_line(&w.summary, 300),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_reply_parses_and_rejects_one_bad_value() {
        let v = verdict(
            &json!({ "passed": false, "findings": [{ "id": "QA-1", "severity": "high",
            "file": "src/a.js", "summary": "crashes on\nempty input" }] }),
        )
        .unwrap();
        assert!(!v.passed && v.findings[0].summary == "crashes on empty input");
        assert!(verdict(
            &json!({ "passed": true, "findings": [{ "id": "x", "severity": "high",
            "file": "../etc/passwd", "summary": "s" }] })
        )
        .is_err());
        assert!(verdict(&json!({ "passed": true })).is_err());
        let many: Vec<Value> = (0..51)
            .map(|i| json!({ "id": format!("Q{i}"), "severity": "low", "summary": "s" }))
            .collect();
        assert!(verdict(&json!({ "passed": true, "findings": many }))
            .unwrap_err()
            .contains("50"));
        let n = notes(&json!({ "summary": "adds", "changes": ["a"], "breaking": false })).unwrap();
        assert_eq!((n.changes.len(), n.breaking), (1, false));
        assert!(notes(
            &json!({ "summary": "x", "changes": [], "breaking": false, "deploy": true })
        )
        .is_err());
        assert!(
            plan(&json!({ "ready": true, "summary": "s" }))
                .unwrap()
                .ready
        );
        assert!(plan(&json!({ "ready": "yes", "summary": "s" })).is_err());
    }
}
