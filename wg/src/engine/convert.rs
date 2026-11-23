//! Kernel JSON ↔ `wg_factory::api` values: memory, and the run's output fields.

use serde_json::{json, Map, Value};
use wg_factory::api::{
    Escalation, Memory, Outcome, Review, Rule, Scope, Size, Stat, Status, Tier, UnitNote,
};

/// An integral float as a JSON integer, as the JS memory file had it.
pub fn num(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

fn scope(s: &str) -> Option<Scope> {
    match s {
        "planner" => Some(Scope::Planner),
        "worker" => Some(Scope::Worker),
        "all" => Some(Scope::All),
        _ => None,
    }
}

fn rule(r: &Value) -> Option<Rule> {
    let pos = |k: &str| r[k].as_f64().filter(|x| x.is_finite() && *x > 0.0);
    let check = r["check"]
        .as_str()
        .map(|c| crate::ids::one_line(c, wg_factory::learn::rules::CHECK_CHARS));
    Some(Rule {
        id: r["id"].as_str()?.into(),
        text: r["text"].as_str()?.into(),
        sig: r["sig"].as_str()?.into(),
        scope: scope(r["scope"].as_str()?)?,
        a: pos("a")?,
        b: pos("b")?,
        check: check.clone(),
        // A name without its check could retire a rule nobody can see the reason for.
        check_name: check.and(
            r["checkName"]
                .as_str()
                .filter(|n| crate::ids::check_name(n).is_ok())
                .map(str::to_string),
        ),
    })
}

/// Memory as run.js read it: malformed rules are dropped (and counted), missing fields default.
pub fn memory_from(m: &Value) -> (Memory, usize) {
    let raw_rules = m["rules"].as_array().cloned().unwrap_or_default();
    let rules: Vec<Rule> = raw_rules.iter().filter_map(rule).collect();
    let dropped = raw_rules.len() - rules.len();
    let models = m["models"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, tiers)| {
                    let ts = Tier::ALL
                        .into_iter()
                        .filter_map(|t| {
                            let s = &tiers[t.name()];
                            Some((
                                t,
                                Stat {
                                    pass: s["pass"].as_f64()?,
                                    fail: s["fail"].as_f64()?,
                                },
                            ))
                        })
                        .collect();
                    (k.clone(), ts)
                })
                .collect()
        })
        .unwrap_or_default();
    let memory = Memory {
        runs: m["runs"].as_u64().unwrap_or(0),
        next_id: m["nextId"].as_u64().unwrap_or(rules.len() as u64 + 1),
        checks: m["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect(),
        rules,
        models,
    };
    (memory, dropped)
}

pub fn rule_json(r: &Rule) -> Value {
    let mut v = json!({ "id": r.id, "text": r.text, "scope": r.scope.name(), "sig": r.sig, "a": num(r.a), "b": num(r.b) });
    if let Some(c) = &r.check {
        v["check"] = json!(c);
    }
    if let Some(n) = &r.check_name {
        v["checkName"] = json!(n);
    }
    v
}

/// Memory v1. Model entries the factory did not touch keep their original JSON.
pub fn memory_to(m: &Memory, original_models: &Value) -> Value {
    let mut models: Map<String, Value> = original_models.as_object().cloned().unwrap_or_default();
    for (key, tiers) in &m.models {
        if tiers.is_empty() {
            continue;
        }
        let entry = models.entry(key.clone()).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        for (t, s) in tiers {
            entry[t.name()] = json!({ "pass": num(s.pass), "fail": num(s.fail) });
        }
    }
    json!({
        "version": 1,
        "runs": m.runs,
        "nextId": m.next_id,
        "checks": m.checks,
        "rules": m.rules.iter().map(rule_json).collect::<Vec<_>>(),
        "models": models,
    })
}

pub fn status(s: Status) -> &'static str {
    match s {
        Status::NothingToDo => "nothing_to_do",
        Status::Stuck => "stuck",
        Status::ReadyForReview => "ready_for_review",
        Status::Failed => "failed",
    }
}

pub fn notes(v: &[UnitNote]) -> Value {
    json!(v
        .iter()
        .map(|n| json!({ "id": n.id, "goal": n.goal, "why": n.why }))
        .collect::<Vec<_>>())
}

pub fn escalations(v: &[Escalation]) -> Value {
    json!(v
        .iter()
        .map(
            |e| json!({ "id": e.id, "severity": e.severity.name(), "question": e.question,
                         "recommendation": e.recommendation, "options": e.options })
        )
        .collect::<Vec<_>>())
}

pub fn review(r: &Review) -> Value {
    json!({
        "passed": r.passed,
        "criteria": r.criteria.iter().map(|c| json!({ "id": c.id, "status": if c.passed { "passed" } else { "failed" }, "evidence": c.evidence })).collect::<Vec<_>>(),
        "findings": r.findings.iter().map(|f| json!({ "id": f.id, "severity": f.severity.name(), "file": f.file, "summary": f.summary })).collect::<Vec<_>>(),
    })
}

/// One row of the rule table: a rule and the check that enforces it (or will).
fn enforcer(r: &Rule) -> Value {
    json!({ "id": r.id, "text": r.text, "check": r.check, "checkName": r.check_name })
}

fn proposal(p: &wg_factory::api::Proposal) -> Value {
    let mut v = json!({ "category": p.category, "proposal": p.proposal, "evidence": p.evidence });
    if let Some(sig) = &p.sig {
        v["sig"] = json!(sig);
    }
    if let Some(n) = &p.check_name {
        v["checkName"] = json!(n);
    }
    v
}

/// The factory's part of the output (design §11.1); the kernel adds its own fields.
pub fn outcome(o: &Outcome) -> Map<String, Value> {
    let size = |s: Size| if s == Size::Large { "large" } else { "small" };
    let w = |r: &Rule| r.a / (r.a + r.b);
    let v = json!({
        "status": status(o.status),
        "note": o.note,
        "review": o.review.as_ref().map(review),
        "done": o.done,
        "skipped": notes(&o.skipped),
        "stuck": notes(&o.stuck),
        "fixRounds": o.fix_rounds,
        "reviewFixRounds": o.review_fix_rounds,
        "outOfScope": o.out_of_scope,
        "userChangesBefore": o.committable,
        "models": o.models.iter().map(|m| json!({ "id": m.id, "size": size(m.size), "tier": m.tier.name() })).collect::<Vec<_>>(),
        "rules": {
            "added": o.rules.added, "revised": o.rules.revised, "pruned": o.rules.pruned,
            "inPlay": o.rules.in_play,
            "top": o.rules.top.iter().map(|r| json!({ "id": r.id, "weight": num(((w(r) * 100.0).round()) / 100.0), "scope": r.scope.name(), "text": r.text })).collect::<Vec<_>>(),
            "stopgaps": o.rules.stopgaps.iter().map(enforcer).collect::<Vec<_>>(),
            "retired": o.rules.retired.iter().map(enforcer).collect::<Vec<_>>(),
        },
        "proposals": o.proposals.iter().map(proposal).collect::<Vec<_>>(),
    });
    v.as_object().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_round_trips_and_keeps_unknown_model_entries() {
        let raw = json!({ "version": 1, "runs": 2, "nextId": 3, "checks": ["npm test", 4],
            "rules": [{ "id": "r1", "text": "t", "scope": "all", "sig": "s", "a": 2, "b": 1 },
                      { "id": "bad", "text": "t", "scope": "boss", "sig": "s", "a": 2, "b": 1 }],
            "models": { "worker:small": { "haiku": { "pass": 1, "fail": 0.5 }, "gpt": { "x": 1 } },
                        "qa:small": "odd" } });
        let (m, dropped) = memory_from(&raw);
        assert_eq!(dropped, 1);
        assert_eq!(m.checks, ["npm test"]);
        let back = memory_to(&m, &raw["models"]);
        assert_eq!(
            back["rules"],
            json!([{ "id": "r1", "text": "t", "scope": "all", "sig": "s", "a": 2, "b": 1 }])
        );
        assert_eq!(
            back["models"]["worker:small"]["haiku"],
            json!({ "pass": 1, "fail": 0.5 })
        );
        assert_eq!(back["models"]["worker:small"]["gpt"], json!({ "x": 1 }));
        assert_eq!(back["models"]["qa:small"], "odd");
        assert!(crate::memory::validate(&back).is_ok());
        let (empty, _) = memory_from(&json!({}));
        assert_eq!((empty.runs, empty.next_id), (0, 1));
    }

    #[test]
    fn a_stopgap_keeps_its_check_through_memory_and_reaches_the_report() {
        let raw = json!({ "version": 1, "runs": 1, "nextId": 2, "checks": [],
            "rules": [{ "id": "r1", "text": "t", "scope": "all", "sig": "s", "a": 2, "b": 1, "check": "eslint ban", "checkName": "no-effect" }],
            "models": {} });
        let (m, _) = memory_from(&raw);
        assert_eq!(m.rules[0].check.as_deref(), Some("eslint ban"));
        let back = memory_to(&m, &raw["models"]);
        assert_eq!(back["rules"], raw["rules"]);
        assert!(crate::memory::validate(&back).is_ok());
        let o = Outcome {
            status: Status::ReadyForReview,
            note: None,
            review: None,
            done: vec![],
            skipped: vec![],
            stuck: vec![],
            fix_rounds: 0,
            review_fix_rounds: 0,
            out_of_scope: vec![],
            committable: vec![],
            models: vec![],
            rules: wg_factory::api::RulesReport {
                stopgaps: m.rules.clone(),
                retired: m.rules.clone(),
                ..Default::default()
            },
            proposals: vec![],
        };
        let row =
            json!([{ "id": "r1", "text": "t", "check": "eslint ban", "checkName": "no-effect" }]);
        assert_eq!(outcome(&o)["rules"]["stopgaps"], row);
        assert_eq!(outcome(&o)["rules"]["retired"], row);
        let (bad, _) = memory_from(
            &json!({ "rules": [{ "id": "r1", "text": "t", "scope": "all",
            "sig": "s", "a": 2, "b": 1, "check": "c", "checkName": "Not A Name" }] }),
        );
        assert_eq!(
            bad.rules[0].check_name, None,
            "an invalid name never matches a check"
        );
        let long = "x".repeat(300) + "\nsecond line";
        let (edited, _) = memory_from(
            &json!({ "rules": [{ "id": "r1", "text": "t", "scope": "all",
            "sig": "s", "a": 2, "b": 1, "check": long, "checkName": "ok" }] }),
        );
        let c = edited.rules[0].check.as_deref().unwrap();
        assert!(c.chars().count() == 240 && !c.contains('\n'));
        assert_eq!(edited.rules[0].check_name.as_deref(), Some("ok"));
        let (orphan, _) = memory_from(
            &json!({ "rules": [{ "id": "r1", "text": "t", "scope": "all",
            "sig": "s", "a": 2, "b": 1, "checkName": "ok" }] }),
        );
        assert_eq!(
            orphan.rules[0].check_name, None,
            "a name never travels without its check"
        );
    }
}
