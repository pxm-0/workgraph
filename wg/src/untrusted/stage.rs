//! Replies of the Phase 4 agent stages: a QA explorer's verdict, release notes and the release
//! writer's word on its plan (the plan itself is in the Haruspex task, checked by the gate).

use super::from_value;
use super::review::{finding, WireFinding};
use crate::ids::one_line;
use serde::Deserialize;
use serde_json::Value;
use wg_factory::api::{Finding, ReleaseNotes, ReleasePlan, Repro, Verdict};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireVerdict {
    passed: bool,
    findings: Vec<WireQaFinding>,
}

/// A QA finding: a review finding plus the repro the kernel reruns (design §6.5).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireQaFinding {
    id: String,
    severity: String,
    #[serde(default)]
    file: Option<String>,
    summary: String,
    repro: WireRepro,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRepro {
    cmd: String,
    output: String,
    expected: String,
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

/// At most 50 findings, each file a repo path, each repro a command the sandbox accepts as a step.
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
            .map(qa_finding)
            .collect::<Result<_, _>>()?,
    })
}

fn qa_finding(w: WireQaFinding) -> Result<Finding, String> {
    let f = finding(WireFinding {
        id: w.id,
        severity: w.severity,
        file: w.file,
        summary: w.summary,
    })?;
    crate::sandbox::job::command(&w.repro.cmd, &format!("finding {}'s repro", f.id))?;
    Ok(Finding {
        repro: Some(Repro {
            cmd: w.repro.cmd,
            output: one_line(&w.repro.output, 1000),
            expected: one_line(&w.repro.expected, 1000),
        }),
        ..f
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
        let repro = json!({ "cmd": "node -e 'require(\"./src/a\")(\"\")'", "output": "TypeError:\nx", "expected": "exit 0" });
        let v = verdict(
            &json!({ "passed": false, "findings": [{ "id": "QA-1", "severity": "high",
            "file": "src/a.js", "summary": "crashes on\nempty input", "repro": repro }] }),
        )
        .unwrap();
        assert!(!v.passed && v.findings[0].summary == "crashes on empty input");
        let r = v.findings[0].repro.as_ref().unwrap();
        assert_eq!(
            (r.output.as_str(), r.expected.as_str()),
            ("TypeError: x", "exit 0")
        );
        assert!(r.cmd.starts_with("node -e"));
        assert!(verdict(
            &json!({ "passed": true, "findings": [{ "id": "x", "severity": "high",
            "file": "../etc/passwd", "summary": "s", "repro": repro }] })
        )
        .is_err());
        assert!(verdict(&json!({ "passed": true })).is_err());
        // A finding needs a repro, and the repro must be one sandbox step: one line, ≤ 4 KB.
        let f = |repro: Value| json!({ "passed": false, "findings": [{ "id": "QA-1", "severity": "high", "summary": "s", "repro": repro }] });
        assert!(verdict(&json!({ "passed": false, "findings": [{ "id": "QA-1", "severity": "high", "summary": "s" }] })).is_err());
        for cmd in ["", "  ", "a\nb", &"x".repeat(4097)] {
            let e = verdict(&f(json!({ "cmd": cmd, "output": "o", "expected": "e" }))).unwrap_err();
            assert!(e.contains("QA-1's repro"), "{e}");
        }
        assert!(verdict(&f(json!({ "cmd": "true", "output": "o" }))).is_err());
        assert!(verdict(&f(
            json!({ "cmd": "true", "output": "o", "expected": "e", "network": true })
        ))
        .is_err());
        let many: Vec<Value> = (0..51)
            .map(|i| json!({ "id": format!("Q{i}"), "severity": "low", "summary": "s", "repro": repro }))
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
