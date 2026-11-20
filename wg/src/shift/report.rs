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

fn output(dirs: &Dirs, run: Option<&str>) -> Value {
    run.and_then(|r| std::fs::read_to_string(dirs.run(r).join("output.json")).ok())
        .and_then(|x| serde_json::from_str(&x).ok())
        .unwrap_or(Value::Null)
}

/// The PR number at the end of a PR URL.
fn pr_number(url: &Value) -> Option<u64> {
    url.as_str()?.rsplit('/').next()?.parse().ok()
}

/// Per run: guard denials, rules added, revised, pruned and rejected, and replayed answers, from
/// its journal, output and guard log.
fn run_notes(dirs: &Dirs, run: &str) -> Value {
    let dir = dirs.run(run);
    let events = journal::read(&dir.join("journal.jsonl")).unwrap_or_default();
    let rules = &output(dirs, Some(run))["rules"];
    let denials = std::fs::read_to_string(dir.join("guard.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("\"decision\":\"deny\""))
        .count();
    json!({
        "guardDenials": denials,
        "rulesAdded": rules["added"].as_array().cloned().unwrap_or_default(),
        "rulesRevised": rules["revised"].as_array().cloned().unwrap_or_default(),
        "rulesPruned": rules["pruned"].as_array().cloned().unwrap_or_default(),
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
    // Every run the shift started, claimed ones too: an interrupted run a resume replaced, or one
    // that ended in a kernel error, still paid for its calls.
    let mut runs = BTreeMap::new();
    for t in of("task_claimed").iter().chain(&tasks) {
        if let Some(r) = t["run"].as_str() {
            runs.insert(r.to_string(), format!("{}#{}", s(t, "repo"), t["issue"]));
        }
    }
    let approvals: Vec<Value> = pick("pr-open")
        .iter()
        .map(|t| {
            let out = output(dirs, t["run"].as_str());
            json!({ "repo": t["repo"], "issue": t["issue"], "title": t["title"], "pr": pr_number(&t["pr"]),
                    "url": t["pr"], "run": t["run"],
                    "verified": out["verified"], "reviewed": out["reviewed"], "review": out["review"],
                    "criteria": out["review"]["criteria"],
                    "checks": out["checks"], "checksAlreadyFailing": out["checksAlreadyFailing"],
                    "risky": out["skipped"], "spend": out["spend"],
                    "tainted": out["tainted"], "riskyBuilt": out["riskyBuilt"], "sandboxRuns": out["sandboxRuns"] })
        })
        .collect();
    let notes: Map<String, Value> = runs
        .keys()
        .map(|r| (r.clone(), run_notes(dirs, r)))
        .collect();
    Ok(json!({
        "shift": id,
        "binary": events.first().map(|e| e["binary"].clone()),
        "staleLockPid": events.first().map(|e| e["stale_lock_pid"].clone()),
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
        "shiftIncidents": of("incident"),
        "sandbox": {
            "doctor": of("sandbox_doctor").last().cloned(),
            "argus": of("argus_fingerprint"),
            "reap": of("sandbox_reap"),
        },
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
        let pr = a["pr"].as_u64().map_or("?".to_string(), |n| n.to_string());
        let criteria: Vec<String> = a["criteria"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| format!("{} {}", s(c, "id"), s(c, "status")))
            .collect();
        let risky: Vec<String> = a["riskyBuilt"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|u| format!("{} ({})", s(u, "id"), s(u, "label")))
            .collect();
        let sandboxed = if a["tainted"] == true {
            format!(
                ", risky units [{}] built and checked only in the sandbox ({} SandboxRun)",
                risky.join(", "),
                a["sandboxRuns"].as_array().map_or(0, Vec::len)
            )
        } else {
            String::new()
        };
        format!("- #{pr} {} ({}#{}): verified {}, reviewed {}, criteria [{}], checks {}, already failing {}{sandboxed}. Approve with `/workgraph:work approve {}#{pr}`",
        crate::ids::one_line(a["title"].as_str().unwrap_or(""), 120), s(a, "repo"), a["issue"], a["verified"], a["reviewed"],
        criteria.join(", "), a["checks"], a["checksAlreadyFailing"], s(a, "repo"))
    };
    let parked = |t: &Value| {
        let resume = if t["resumable"] == true {
            format!(" Resume with `wg shift resume {}`.", s(r, "shift"))
        } else {
            String::new()
        };
        format!("{}{resume}", task_line(t))
    };
    let rules: Vec<String> = r["runs"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(run, n)| {
            let ids = |k: &str| {
                n[k].as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| crate::ids::one_line(x.as_str().unwrap_or("?"), 64))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "- {run}: added [{}], revised [{}], pruned [{}], rejected [{}]",
                ids("rulesAdded"),
                ids("rulesRevised"),
                ids("rulesPruned"),
                ids("rulesRejected")
            )
        })
        .collect();
    let sb = &r["sandbox"];
    let fp = |at: &str| {
        sb["argus"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["at"] == at)
            .and_then(|e| e["fingerprint"]["sha256"].as_str())
            .map_or("-".to_string(), |h| h[..h.len().min(12)].to_string())
    };
    let sandbox = format!(
        "Doctor: {}. vm_host_reach: accepted (owner decision D1: a container escape lands in the sim VM).\nArgus fingerprint: start {}, end {}.",
        match sb["doctor"]["ok"].as_bool() {
            Some(true) => "ok".to_string(),
            _ => format!("risky units park ({})", crate::ids::one_line(sb["doctor"]["failed"].as_str().unwrap_or("not run"), 200)),
        },
        fp("start"),
        fp("end")
    );
    let shift_incidents = list(&r["shiftIncidents"], &|e| {
        format!(
            "- shift: {}",
            crate::ids::one_line(e["why"].as_str().unwrap_or(""), 300)
        )
    });
    let stale = match r["staleLockPid"].as_u64() {
        Some(pid) => format!("\n\nThe previous shift (pid {pid}) died holding the lock."),
        None => String::new(),
    };
    format!(
        "# Shift {}\n\nStopped: {}{stale}\n\n## Needs you\n\n### Incidents\n{}\n{}\n\n### Decisions\n{}\n\n### Approvals\n{}\n\n### Parked\n{}\n\n## Stuck\n{}\n\n## Skipped\n{}\n{}\n\n## Sandbox\n\n{}\n\n## Rules\n{}\n\n## Spend\n\nTotal ${} (calls with unknown cost: {})\n\n```json\n{}\n```\n",
        s(r, "shift"),
        r["stopped"],
        list(&n["incidents"], &task_line),
        if r["shiftIncidents"].as_array().is_some_and(|a| !a.is_empty()) { shift_incidents } else { String::new() },
        list(&n["decisions"], &decision),
        list(&n["approvals"], &approval),
        list(&n["parked"], &parked),
        list(&r["stuck"], &task_line),
        list(&r["skippedRepos"], &|e| format!("- repo {}: {}", s(e, "repo"), s(e, "why"))),
        list(&r["skippedIssues"], &|e| format!("- {}#{}: {}", s(e, "repo"), e["issue"], s(e, "why"))),
        sandbox,
        if rules.is_empty() { "- none".to_string() } else { rules.join("\n") },
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
        "needsYou": { "incidents": count("incidents") + r["shiftIncidents"].as_array().map_or(0, Vec::len),
                      "decisions": count("decisions"),
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
        j.append(
            "shift_started",
            json!({ "binary": "b1", "stale_lock_pid": 4242 }),
        )
        .unwrap();
        j.append(
            "task_claimed",
            json!({ "repo": "o/r", "issue": 5, "run": "r-5" }),
        )
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
                led("r-5", "subscription", json!(2000)),
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::create_dir_all(dirs.run("r-1")).unwrap();
        std::fs::write(
            dirs.run("r-1").join("output.json"),
            r#"{"verified":true,"reviewed":true,"checks":["test"],"review":{"criteria":[{"id":"AC-1","status":"passed"}]},"rules":{"added":["r9"],"revised":[],"pruned":["r2"]}}"#,
        )
        .unwrap();
        let out = write(&dirs, "20261120-0130").unwrap();
        assert_eq!(
            out["needsYou"],
            json!({ "incidents": 1, "decisions": 1, "approvals": 1, "parked": 1 })
        );
        assert_eq!(
            out["spendUsd"],
            json!(0.0035),
            "a claimed run that never finished still counts"
        );
        let r: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(r["spend"]["callsWithUnknownCost"], 1);
        assert_eq!(r["spend"]["byProvider"]["subscription/opus"]["calls"], 2);
        assert_eq!(
            r["spend"]["byTask"]["o/r#2/anthropic-api"]["usd"],
            json!(0.0005)
        );
        assert_eq!(r["needsYou"]["approvals"][0]["verified"], true);
        assert_eq!(r["needsYou"]["approvals"][0]["pr"], 7);
        assert_eq!(r["staleLockPid"], 4242);
        assert_eq!(r["runs"]["r-1"]["rulesAdded"], json!(["r9"]));
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
        assert!(
            md.contains("- #7 ") && md.contains("/workgraph:work approve o/r#7`"),
            "{md}"
        );
        assert!(md.contains("AC-1 passed") && md.contains("pid 4242"));
        assert!(md.contains("- r-1: added [r9], revised [], pruned [r2]"));
        assert_eq!(latest(&dirs).unwrap().as_deref(), Some("20261120-0130"));
    }
}
