//! The replay invariants (design §12.4) and the suite over a base checkout's cases. Cases and
//! `eval/manifest.toml` come from base, and the number of cases replayed must equal base's count,
//! so moving or deleting cassettes in the head changes nothing.

use super::replay::{self, Case, Replayed};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub replay_cases: usize,
    pub store: Store,
}

/// The Tier B fixture store pin (design §12.4).
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Store {
    pub repo: String,
    /// The pinned store commit; empty until the owner seeds the store and pins it.
    pub commit: String,
    /// sha256 of `git ls-tree -r <commit>` at the pin.
    pub tree_sha256: String,
    pub min_fixtures: usize,
    pub min_needs_decision: usize,
}

pub const COMPILED: &str = include_str!("../../../eval/manifest.toml");

pub fn manifest(text: &str) -> Result<Manifest, String> {
    toml::from_str(text).map_err(|e| format!("eval/manifest.toml: {e}"))
}

fn high(v: &Value) -> bool {
    matches!(v["severity"].as_str(), Some("high") | Some("critical"))
}

fn edits(label: &str) -> bool {
    label.starts_with("work:")
        || (label.contains(':') && label.contains('#') && !label.starts_with("review"))
}

/// Every invariant `r` breaks.
pub fn violations(case: &Case, r: &Replayed) -> Vec<String> {
    let mut out = Vec::new();
    let status = r.result["status"].as_str().unwrap_or("");
    if status != case.expect.status {
        out.push(format!(
            "terminal status {status:?}, expected {:?}",
            case.expect.status
        ));
    }
    let llm: Vec<&Value> = r
        .journal
        .iter()
        .filter(|e| e["kind"] == "command" && e["what"] == "llm")
        .collect();
    let escalated = ["read", "challenge"].iter().any(|l| {
        case.replies.get(*l).is_some_and(|v| {
            v["escalations"]
                .as_array()
                .is_some_and(|a| a.iter().any(high))
        })
    });
    if escalated {
        if let Some(c) = llm
            .iter()
            .find(|c| edits(c["label"].as_str().unwrap_or("")))
        {
            out.push(format!(
                "{} ran while a high or critical escalation had no decision",
                c["label"]
            ));
        }
    }
    let runs: Vec<&Value> = r
        .journal
        .iter()
        .filter(|e| e["kind"] == "checks_run")
        .collect();
    if let Some(base) = runs.iter().find(|e| e["label"] == "check:baseline") {
        let failed: Vec<&str> = base["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|x| x["exit"] != 0)
            .filter_map(|x| x["name"].as_str())
            .collect();
        let gate: Vec<&String> = case
            .checks
            .keys()
            .filter(|n| !failed.contains(&n.as_str()))
            .collect();
        for run in runs.iter().filter(|e| e["label"] != "check:baseline") {
            let names: Vec<&str> = run["results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x["name"].as_str())
                .collect();
            if let Some(missing) = gate.iter().find(|g| !names.contains(&g.as_str())) {
                out.push(format!(
                    "{} left the gate check {missing} out",
                    run["label"]
                ));
            }
        }
    } else if llm.iter().any(|c| edits(c["label"].as_str().unwrap_or(""))) {
        out.push("a Worker ran without the kernel's baseline".into());
    }
    if status == "ready_for_review"
        && !llm.iter().any(|c| {
            c["label"]
                .as_str()
                .is_some_and(|l| l.starts_with("review#"))
                && c["tier"] == "opus"
        })
    {
        out.push("the run finished without an opus review".into());
    }
    if let Some(m) = &r.memory {
        if let Err(e) = crate::memory::validate(m) {
            out.push(format!("saved memory is not v1: {e}"));
        }
        for rule in m["rules"].as_array().into_iter().flatten() {
            if let Err(e) = crate::untrusted::rules::sanitize(rule["text"].as_str().unwrap_or("")) {
                out.push(format!("a saved rule fails the sanitizer: {e}"));
            }
        }
    }
    out
}

/// Replays every case under `cases` (base's) with this build's engine; the count must equal the
/// manifest's (base's).
pub fn suite(cases: &Path, m: &Manifest, scratch: &Path) -> Result<Value, String> {
    let mut dirs: Vec<_> = std::fs::read_dir(cases)
        .map_err(|e| format!("cannot read {}: {e}", cases.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("case.json").is_file())
        .collect();
    dirs.sort();
    if dirs.len() != m.replay_cases {
        return Err(format!(
            "{} replay cases found, the manifest pins {}",
            dirs.len(),
            m.replay_cases
        ));
    }
    let mut results = Vec::new();
    let mut failed = 0;
    for d in &dirs {
        let name = d
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let case = replay::load(&d.join("case.json"))?;
        let home = scratch.join(&name);
        if home.exists() {
            std::fs::remove_dir_all(&home).map_err(|e| e.to_string())?;
        }
        let v = match replay::run(&case, &home, &wg_factory::engine::step) {
            Ok(r) => violations(&case, &r),
            Err(e) => vec![format!("the replay failed: {e}")],
        };
        failed += usize::from(!v.is_empty());
        results.push(json!({ "case": name, "ok": v.is_empty(), "violations": v }));
    }
    Ok(json!({ "cases": dirs.len(), "failed": failed, "results": results }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_invariant_catches_one_planted_breach() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../eval/replay");
        let esc = replay::load(&root.join("critical-escalation/case.json")).unwrap();
        let happy = replay::load(&root.join("happy-path/case.json")).unwrap();
        let r = |status: &str, journal: Vec<Value>, memory: Option<Value>| Replayed {
            result: json!({ "status": status }),
            journal,
            memory,
        };
        let llm = |label: &str, tier: &str| json!({ "kind": "command", "what": "llm", "label": label, "tier": tier });
        let checks = |label: &str, names: &[(&str, i64)]| {
            json!({ "kind": "checks_run", "label": label,
            "results": names.iter().map(|(n, e)| json!({ "name": n, "exit": e })).collect::<Vec<_>>() })
        };
        let base = checks("check:baseline", &[("test", 0)]);
        for (case, run, want) in [
            (
                &esc,
                r("needs_decision", vec![llm("work:a", "sonnet")], None),
                "had no decision",
            ),
            (
                &happy,
                r("needs_decision", vec![], None),
                "expected \"ready_for_review\"",
            ),
            (
                &happy,
                r(
                    "ready_for_review",
                    vec![
                        base.clone(),
                        checks("check:L1", &[]),
                        llm("review#1", "opus"),
                    ],
                    None,
                ),
                "left the gate check test out",
            ),
            (
                &happy,
                r(
                    "ready_for_review",
                    vec![llm("work:a", "haiku"), llm("review#1", "opus")],
                    None,
                ),
                "without the kernel's baseline",
            ),
            (
                &happy,
                r(
                    "ready_for_review",
                    vec![base.clone(), llm("review#1", "sonnet")],
                    None,
                ),
                "without an opus review",
            ),
            (
                &happy,
                r(
                    "ready_for_review",
                    vec![base.clone(), llm("review#1", "opus")],
                    Some(json!({ "version": 2 })),
                ),
                "not v1",
            ),
        ] {
            let v = violations(case, &run);
            assert!(v.iter().any(|x| x.contains(want)), "{want}: {v:?}");
        }
        assert!(violations(
            &happy,
            &r(
                "ready_for_review",
                vec![base, llm("review#1", "opus")],
                None
            )
        )
        .is_empty());
    }

    #[test]
    fn the_shipped_cases_replay_clean_and_the_count_is_pinned() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let m = manifest(COMPILED).unwrap();
        let scratch = std::env::temp_dir().join(format!("wg-replay-suite-{}", std::process::id()));
        let out = suite(&root.join("eval/replay"), &m, &scratch).unwrap();
        assert_eq!(out["failed"], 0, "{out}");
        let fewer = Manifest {
            replay_cases: m.replay_cases + 1,
            ..m
        };
        assert!(suite(&root.join("eval/replay"), &fewer, &scratch)
            .unwrap_err()
            .contains("pins"));
    }
}
