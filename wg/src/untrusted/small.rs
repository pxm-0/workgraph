//! Challenge, Work and Learn replies.

use super::{escalation, from_value, Ctx, WireEscalation};
use crate::ids::one_line;
use serde::Deserialize;
use serde_json::Value;
use wg_factory::api::{Added, Assign, Challenge, Learn, Proposal, RuleDraft, Scope, Work};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireChallenge {
    added: Vec<WireAdded>,
    assign: Vec<WireAssign>,
    escalations: Vec<WireEscalation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAdded {
    id: String,
    #[serde(default)]
    kind: Option<String>,
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WireAssign {
    criterion_id: String,
    unit_id: String,
}

/// Assignments must name a criterion the Reading or this challenge defines. Unknown unit ids are
/// kept: the engine logs them and leaves those criteria to the reviewer, as run.js did.
pub fn challenge(v: &Value, ctx: &Ctx) -> Result<Challenge, String> {
    let w: WireChallenge = from_value(v, "the challenge")?;
    let added: Vec<Added> = w
        .added
        .into_iter()
        .map(|a| Added {
            id: one_line(&a.id, 32),
            kind: a.kind.map(|k| one_line(&k, 40)),
            summary: one_line(&a.summary, 500),
        })
        .collect();
    for a in &w.assign {
        let known = ctx.known_criteria.contains(&a.criterion_id)
            || added.iter().any(|x| x.id == a.criterion_id);
        if !known {
            return Err(format!(
                "assignment names unknown criterion {:?}",
                one_line(&a.criterion_id, 32)
            ));
        }
    }
    Ok(Challenge {
        added,
        assign: w
            .assign
            .into_iter()
            .map(|a| Assign {
                criterion_id: one_line(&a.criterion_id, 32),
                unit_id: one_line(&a.unit_id, 32),
            })
            .collect(),
        escalations: w
            .escalations
            .into_iter()
            .map(escalation)
            .collect::<Result<_, _>>()?,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireWork {
    summary: String,
    #[serde(default)]
    blocked: Option<String>,
}

pub fn work(v: &Value) -> Result<Work, String> {
    let w: WireWork = from_value(v, "the work reply")?;
    Ok(Work {
        summary: one_line(&w.summary, 2000),
        blocked: w
            .blocked
            .filter(|b| !b.trim().is_empty())
            .map(|b| one_line(&b, 2000)),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLearn {
    rules: Vec<WireRule>,
    proposals: Vec<WireProposal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRule {
    sig: String,
    text: String,
    scope: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProposal {
    category: String,
    proposal: String,
    evidence: String,
    #[serde(default)]
    sig: Option<String>,
}

/// The retro categories, plus `factory` (something workgraph itself did badly, design §10.4):
/// filed as `wg:self-proposed` issues, never applied and never taken by the self lane.
const CATEGORIES: [&str; 5] = [
    "automated-check",
    "coding-standard",
    "navigation",
    "haruspex-profile",
    "factory",
];

/// Every sig must be one this run fixed. Rule text is sanitized later, rule by rule, so one bad
/// rule does not cost the proposals (design §10.3).
pub fn learn(v: &Value, ctx: &Ctx) -> Result<Learn, String> {
    let w: WireLearn = from_value(v, "the learn reply")?;
    let rules = w
        .rules
        .into_iter()
        .map(|r| {
            if !ctx.fixed_sigs.contains(&r.sig) {
                return Err("a rule names a signature this run did not fix".to_string());
            }
            let scope = match r.scope.as_str() {
                "planner" => Scope::Planner,
                "worker" => Scope::Worker,
                "all" => Scope::All,
                _ => return Err("a rule has an unknown scope".to_string()),
            };
            Ok(RuleDraft {
                sig: r.sig,
                text: r.text,
                scope,
                check: None,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let proposals = w
        .proposals
        .into_iter()
        .map(|mut p| {
            p.sig = p.sig.filter(|s| !s.trim().is_empty());
            if !CATEGORIES.contains(&p.category.as_str()) {
                return Err("a proposal has an unknown category".to_string());
            }
            if let Some(sig) = &p.sig {
                if p.category != "automated-check" {
                    return Err("only an automated-check proposal may name a signature".to_string());
                }
                if !ctx.fixed_sigs.contains(sig) {
                    return Err("a proposal names a signature this run did not fix".to_string());
                }
            }
            Ok(Proposal {
                category: p.category,
                proposal: one_line(&p.proposal, 1000),
                evidence: one_line(&p.evidence, 1000),
                sig: p.sig,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Learn { rules, proposals })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Ctx {
        Ctx {
            known_criteria: vec!["AC-1".into()],
            fixed_sigs: vec!["npm test | x".into()],
            ..Ctx::default()
        }
    }

    #[test]
    fn challenge_assignments_need_known_criteria() {
        let ok = json!({ "added": [{ "id": "AC-9", "summary": "s" }],
                         "assign": [{ "criterionId": "AC-9", "unitId": "zz" }, { "criterionId": "AC-1", "unitId": "a" }],
                         "escalations": [] });
        assert_eq!(challenge(&ok, &ctx()).unwrap().assign.len(), 2);
        let bad = json!({ "added": [], "assign": [{ "criterionId": "AC-7", "unitId": "a" }], "escalations": [] });
        assert!(challenge(&bad, &ctx())
            .unwrap_err()
            .contains("unknown criterion"));
    }

    #[test]
    fn work_blocked_empty_means_not_blocked() {
        assert_eq!(
            work(&json!({ "summary": "s", "blocked": " " }))
                .unwrap()
                .blocked,
            None
        );
        assert!(work(&json!({ "summary": "s", "x": 1 })).is_err());
    }

    #[test]
    fn learn_rejects_unknown_sigs_scopes_and_categories() {
        let rule = |sig: &str, scope: &str| json!({ "sig": sig, "text": "t", "scope": scope });
        let prop = |c: &str| json!({ "category": c, "proposal": "p", "evidence": "e" });
        let ok =
            json!({ "rules": [rule("npm test | x", "worker")], "proposals": [prop("navigation")] });
        assert!(learn(&ok, &ctx()).is_ok());
        let factory = json!({ "rules": [], "proposals": [prop("factory")] });
        assert_eq!(
            learn(&factory, &ctx()).unwrap().proposals[0].category,
            "factory"
        );
        for bad in [
            json!({ "rules": [rule("other", "worker")], "proposals": [] }),
            json!({ "rules": [rule("npm test | x", "boss")], "proposals": [] }),
            json!({ "rules": [], "proposals": [prop("deploy-now")] }),
        ] {
            assert!(learn(&bad, &ctx()).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_an_automated_check_may_link_a_fixed_signature() {
        let prop = |c: &str, sig: &str| json!({ "category": c, "proposal": "p", "evidence": "e", "sig": sig });
        let ok = json!({ "rules": [], "proposals": [prop("automated-check", "npm test | x")] });
        assert_eq!(
            learn(&ok, &ctx()).unwrap().proposals[0].sig.as_deref(),
            Some("npm test | x")
        );
        let judgement =
            json!({ "rules": [], "proposals": [prop("coding-standard", "npm test | x")] });
        assert!(learn(&judgement, &ctx())
            .unwrap_err()
            .contains("only an automated-check"));
        let blank = json!({ "rules": [], "proposals": [prop("navigation", " ")] });
        assert_eq!(learn(&blank, &ctx()).unwrap().proposals[0].sig, None);
        let unknown = json!({ "rules": [], "proposals": [prop("automated-check", "other")] });
        assert!(learn(&unknown, &ctx()).unwrap_err().contains("did not fix"));
    }
}
