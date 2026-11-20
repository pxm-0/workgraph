//! The shift report (design §7.5): `~/.workgraph/shifts/<id>/report.json` and `report.md`, built
//! from the shift journal, the run journals and outputs and the cost ledger only, so `wg shift
//! report <id>` regenerates it. "Needs you" comes first: incidents, decisions, approvals, parked.

use crate::journal;
use crate::state_dirs::Dirs;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

fn usd(micros: u64) -> f64 {
    micros as f64 / 1_000_000.0
}

/// `{key: {calls, usd, unknownCost}}` over ledger lines grouped by `key`.
fn group(lines: &[Value], key: impl Fn(&Value) -> String) -> Value {
    let mut by: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    for l in lines {
        let e = by.entry(key(l)).or_default();
        e.0 += 1;
        match l["cost_micros"].as_u64() {
            Some(c) => e.1 += c,
            None => e.2 += 1,
        }
    }
    Value::Object(
        by.into_iter()
            .map(|(k, (calls, m, unknown))| {
                (
                    k,
                    json!({ "calls": calls, "usd": usd(m), "unknownCost": unknown }),
                )
            })
            .collect::<Map<_, _>>(),
    )
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("?").to_string()
}

/// Spend of the shift's runs, per call, stage, task and shift, each split by provider.
fn spend(dirs: &Dirs, runs: &BTreeMap<String, String>) -> Result<Value, String> {
    let mut lines = Vec::new();
    for (run, task) in runs {
        let (_, _, ls) = crate::budget::run_spend(&dirs.ledger(), run)?;
        lines.extend(ls.into_iter().map(|mut l| {
            l["task"] = json!(task);
            l
        }));
    }
    let known: u64 = lines.iter().filter_map(|l| l["cost_micros"].as_u64()).sum();
    let unknown = lines.iter().filter(|l| !l["cost_micros"].is_u64()).count();
    Ok(json!({
        "usd": usd(known),
        "callsWithUnknownCost": unknown,
        "byProvider": group(&lines, |l| format!("{}/{}", s(l, "provider"), s(l, "model_id"))),
        "byStage": group(&lines, |l| format!("{}/{}", s(l, "stage"), s(l, "provider"))),
        "byTask": group(&lines, |l| format!("{}/{}", s(l, "task"), s(l, "provider"))),
        "fallbackSwitches": lines.iter().filter(|l| l["switched_after"].is_string())
            .map(|l| json!({ "run": l["run"], "label": l["label"], "cause": l["switched_after"] })).collect::<Vec<_>>(),
        "calls": lines.iter().map(|l| json!({ "task": l["task"], "run": l["run"], "label": l["label"],
            "stage": l["stage"], "provider": l["provider"], "model_id": l["model_id"], "cost_micros": l["cost_micros"] }))
            .collect::<Vec<_>>(),
    }))
}

/// Per run: guard denials, rejected rules and warnings, from its journal and guard log.
fn run_notes(dirs: &Dirs, run: &str) -> Value {
    let dir = dirs.run(run);
    let events = journal::read(&dir.join("journal.jsonl")).unwrap_or_default();
    let denials = std::fs::read_to_string(dir.join("guard.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("\"decision\":\"deny\""))
        .count();
    json!({
        "guardDenials": denials,
        "rulesRejected": events.iter().filter(|e| e["kind"] == "rule_rejected").map(|e| e["rule"].clone()).collect::<Vec<_>>(),
        "replayed": events.iter().filter(|e| e["kind"] == "llm_replayed").count(),
    })
}

pub fn build(dirs: &Dirs, id: &str) -> Result<Value, String> {
    let events = journal::read(&dirs.shifts().join(id).join("journal.jsonl"))?;
    let of = |kind: &str| {
        events
            .iter()
            .filter(|e| e["kind"] == kind)
            .cloned()
            .collect::<Vec<_>>()
    };
    let tasks = of("task_finished");
    let pick = |outcome: &str| {
        tasks
            .iter()
            .filter(|t| t["outcome"] == outcome)
            .cloned()
            .collect::<Vec<_>>()
    };
    let mut runs = BTreeMap::new();
    for t in &tasks {
        if let Some(r) = t["run"].as_str() {
            runs.insert(r.to_string(), format!("{}#{}", s(t, "repo"), t["issue"]));
        }
    }
    let approvals: Vec<Value> = pick("pr-open")
        .iter()
        .map(|t| {
            let out: Value = t["run"]
                .as_str()
                .and_then(|r| std::fs::read_to_string(dirs.run(r).join("output.json")).ok())
                .and_then(|x| serde_json::from_str(&x).ok())
                .unwrap_or(Value::Null);
            json!({ "repo": t["repo"], "issue": t["issue"], "pr": t["pr"], "run": t["run"],
                    "verified": out["verified"], "reviewed": out["reviewed"], "review": out["review"],
                    "checks": out["checks"], "checksAlreadyFailing": out["checksAlreadyFailing"],
                    "risky": out["skipped"], "spend": out["spend"] })
        })
        .collect();
    let notes: Map<String, Value> = runs
        .keys()
        .map(|r| (r.clone(), run_notes(dirs, r)))
        .collect();
    Ok(json!({
        "shift": id,
        "binary": events.first().map(|e| e["binary"].clone()),
        "stopped": of("stopped").iter().map(|e| e["why"].clone()).collect::<Vec<_>>(),
        "needsYou": {
            "incidents": pick("incident"),
            "decisions": pick("needs-decision"),
            "approvals": approvals,
            "parked": pick("parked"),
        },
        "tasks": tasks,
        "stuck": pick("stuck"),
        "skippedRepos": of("repo_skipped"),
        "skippedIssues": of("issue_skipped"),
        "errors": of("error"),
        "runs": notes,
        "spend": spend(dirs, &runs)?,
    }))
}

fn task_line(t: &Value) -> String {
    format!(
        "- {}#{} ({}): {}",
        s(t, "repo"),
        t["issue"],
        t["run"].as_str().unwrap_or("-"),
        crate::ids::one_line(t["reason"].as_str().unwrap_or(""), 300)
    )
}

pub fn markdown(r: &Value) -> String {
    let list = |v: &Value, f: &dyn Fn(&Value) -> String| {
        let items: Vec<String> = v.as_array().into_iter().flatten().map(f).collect();
        if items.is_empty() {
            "- none".to_string()
        } else {
            items.join("\n")
        }
    };
    let n = &r["needsYou"];
    let decision = |t: &Value| {
        let qs = list(&t["escalations"], &|e| {
            format!(
                "  - {} ({}): recommended {}; {}",
                s(e, "id"),
                s(e, "severity"),
                crate::ids::one_line(e["recommendation"].as_str().unwrap_or(""), 200),
                crate::ids::one_line(e["question"].as_str().unwrap_or(""), 400)
            )
        });
        format!(
            "- {}#{}: answer with `/workgraph:shift decide {}#{}`\n{qs}",
            s(t, "repo"),
            t["issue"],
            s(t, "repo"),
            t["issue"]
        )
    };
    let approval = |a: &Value| {
        format!("- {} ({}#{}): verified {}, reviewed {}, checks {}, already failing {}. Approve with `/workgraph:work approve {}#<pr>`",
        a["pr"].as_str().unwrap_or("?"), s(a, "repo"), a["issue"], a["verified"], a["reviewed"], a["checks"], a["checksAlreadyFailing"], s(a, "repo"))
    };
    format!(
        "# Shift {}\n\nStopped: {}\n\n## Needs you\n\n### Incidents\n{}\n\n### Decisions\n{}\n\n### Approvals\n{}\n\n### Parked\n{}\n\n## Stuck\n{}\n\n## Skipped\n{}\n{}\n\n## Spend\n\nTotal ${} (calls with unknown cost: {})\n\n```json\n{}\n```\n",
        s(r, "shift"),
        r["stopped"],
        list(&n["incidents"], &task_line),
        list(&n["decisions"], &decision),
        list(&n["approvals"], &approval),
        list(&n["parked"], &task_line),
        list(&r["stuck"], &task_line),
        list(&r["skippedRepos"], &|e| format!("- repo {}: {}", s(e, "repo"), s(e, "why"))),
        list(&r["skippedIssues"], &|e| format!("- {}#{}: {}", s(e, "repo"), e["issue"], s(e, "why"))),
        r["spend"]["usd"],
        r["spend"]["callsWithUnknownCost"],
        serde_json::to_string_pretty(&json!({ "byProvider": r["spend"]["byProvider"], "byStage": r["spend"]["byStage"],
            "byTask": r["spend"]["byTask"] })).unwrap_or_default(),
    )
}

/// Builds and writes both files; the one JSON line summarizes them.
pub fn write(dirs: &Dirs, id: &str) -> Result<Value, String> {
    let r = build(dirs, id)?;
    let dir = dirs.shifts().join(id);
    std::fs::write(
        dir.join("report.json"),
        serde_json::to_string_pretty(&r).expect("serializes"),
    )
    .map_err(|e| format!("cannot write the shift report: {e}"))?;
    std::fs::write(dir.join("report.md"), markdown(&r))
        .map_err(|e| format!("cannot write the shift report: {e}"))?;
    let count = |k: &str| r["needsYou"][k].as_array().map_or(0, Vec::len);
    Ok(json!({
        "shift": id,
        "report": dir.join("report.md"),
        "tasks": r["tasks"].as_array().map_or(0, Vec::len),
        "stopped": r["stopped"],
        "needsYou": { "incidents": count("incidents"), "decisions": count("decisions"),
                      "approvals": count("approvals"), "parked": count("parked") },
        "spendUsd": r["spend"]["usd"],
    }))
}

/// The newest shift id, by name (they sort by time).
pub fn latest(dirs: &Dirs) -> Result<Option<String>, String> {
    let entries = match std::fs::read_dir(dirs.shifts()) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", dirs.shifts().display())),
    };
    let mut ids: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("journal.jsonl").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    Ok(ids.pop())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;

    #[test]
    fn the_report_puts_what_needs_the_owner_first_and_splits_spend_by_provider() {
        let home = std::env::temp_dir().join(format!("wg-shift-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dirs = Dirs { home };
        assert_eq!(latest(&dirs).unwrap(), None);
        let dir = dirs.shifts().join("20261120-0130");
        std::fs::create_dir_all(&dir).unwrap();
        let j = Journal::open(&dir.join("journal.jsonl"), "20261120-0130").unwrap();
        j.append("shift_started", json!({ "binary": "b1" }))
            .unwrap();
        j.append(
            "issue_skipped",
            json!({ "repo": "o/r", "issue": 9, "why": "labelled wg:queued by stranger" }),
        )
        .unwrap();
        for (n, outcome, run) in [
            (1, "pr-open", "r-1"),
            (2, "needs-decision", "r-2"),
            (3, "incident", "r-3"),
            (4, "parked", "r-4"),
        ] {
            j.append("task_finished", json!({ "repo": "o/r", "issue": n, "run": run, "outcome": outcome, "reason": "x",
                "pr": if n == 1 { json!("https://x/pull/7") } else { json!(null) },
                "escalations": [{ "id": "E1", "severity": "high", "question": "q?", "recommendation": "r" }] })).unwrap();
        }
        let led = |run: &str, provider: &str, cost: Value| {
            json!({ "run": run, "label": "read", "stage": "read",
            "provider": provider, "model_id": "opus", "cost_micros": cost })
            .to_string()
        };
        std::fs::create_dir_all(dirs.state()).unwrap();
        std::fs::write(
            dirs.ledger(),
            [
                led("r-1", "subscription", json!(1000)),
                led("r-2", "anthropic-api", json!(500)),
                led("r-2", "openrouter", json!("unknown")),
                led("other", "subscription", json!(9)),
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::create_dir_all(dirs.run("r-1")).unwrap();
        std::fs::write(
            dirs.run("r-1").join("output.json"),
            r#"{"verified":true,"reviewed":true,"checks":["test"]}"#,
        )
        .unwrap();
        let out = write(&dirs, "20261120-0130").unwrap();
        assert_eq!(
            out["needsYou"],
            json!({ "incidents": 1, "decisions": 1, "approvals": 1, "parked": 1 })
        );
        assert_eq!(out["spendUsd"], json!(0.0015));
        let r: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(r["spend"]["callsWithUnknownCost"], 1);
        assert_eq!(r["spend"]["byProvider"]["subscription/opus"]["calls"], 1);
        assert_eq!(
            r["spend"]["byTask"]["o/r#2/anthropic-api"]["usd"],
            json!(0.0005)
        );
        assert_eq!(r["needsYou"]["approvals"][0]["verified"], true);
        let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
        let at = |s: &str| md.find(s).unwrap_or_else(|| panic!("{s} missing:\n{md}"));
        assert!(
            at("### Incidents") < at("### Decisions")
                && at("### Decisions") < at("### Approvals")
                && at("### Approvals") < at("### Parked")
                && at("### Parked") < at("## Spend")
        );
        assert!(
            md.contains("/workgraph:shift decide o/r#2")
                && md.contains("labelled wg:queued by stranger")
        );
        assert_eq!(latest(&dirs).unwrap().as_deref(), Some("20261120-0130"));
    }
}
