//! Weighted rules: credit for rules in play, birth from fixed failures, rewrite, prune and cap.

use super::cap_pair;
use crate::api::{Rule, RuleDraft, Scope};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const RULES_PER_PROMPT: usize = 8;
/// Rules below this stay in memory but out of prompts.
pub const MIN_WEIGHT: f64 = 0.4;
/// ...and are deleted once they have 4+ observations.
pub const PRUNE_BELOW: f64 = 0.3;
pub const MAX_RULES: usize = 30;
/// Credit when a rule was in play and its failure stayed away.
pub const HELD: f64 = 0.25;
/// Penalty when its failure came back anyway.
pub const RECURRED: f64 = 1.0;

pub fn weight(r: &Rule) -> f64 {
    r.a / (r.a + r.b)
}

fn by_weight_desc(rules: &mut [Rule]) {
    rules.sort_by(|x, y| {
        weight(y)
            .partial_cmp(&weight(x))
            .unwrap_or(core::cmp::Ordering::Equal)
    });
}

/// The strongest rules for a role's prompt, strongest first.
pub fn for_prompt(rules: &[Rule], role: Scope) -> Vec<Rule> {
    let mut rs: Vec<Rule> = rules
        .iter()
        .filter(|r| (r.scope == role || r.scope == Scope::All) && weight(r) >= MIN_WEIGHT)
        .cloned()
        .collect();
    by_weight_desc(&mut rs);
    rs.truncate(RULES_PER_PROMPT);
    rs
}

/// A rule in play gains a little when its failure stays away and loses a lot when it recurs.
pub fn credit(rules: &[Rule], in_play: &[String], recurred: &[String]) -> Vec<Rule> {
    rules
        .iter()
        .map(|r| {
            if !in_play.contains(&r.id) {
                return r.clone();
            }
            let (a, b) = if recurred.contains(&r.sig) {
                cap_pair(r.a, r.b + RECURRED)
            } else {
                cap_pair(r.a + HELD, r.b)
            };
            Rule { a, b, ..r.clone() }
        })
        .collect()
}

/// The learn step's result: the next rule list and what changed.
pub struct Learned {
    pub rules: Vec<Rule>,
    pub next_id: u64,
    pub added: Vec<String>,
    pub revised: Vec<String>,
    pub pruned: Vec<String>,
}

/// Rewrites rules whose failure recurred, births new ones (a = 2, b = 1: born from a fix that
/// worked), prunes weak rules that had a fair trial and keeps the strongest `MAX_RULES`.
pub fn apply(scored: &[Rule], phrased: &[RuleDraft], next_id: u64) -> Learned {
    let mut revised = Vec::new();
    let rewritten: Vec<Rule> = scored
        .iter()
        .map(|r| match phrased.iter().find(|p| p.sig == r.sig) {
            Some(p) => {
                revised.push(r.id.clone());
                Rule {
                    text: String::from(p.text.trim()),
                    scope: p.scope,
                    ..r.clone()
                }
            }
            None => r.clone(),
        })
        .collect();
    let mut next = next_id;
    let mut added = Vec::new();
    let mut fresh = Vec::new();
    for (k, p) in phrased.iter().enumerate() {
        let first = phrased.iter().position(|q| q.sig == p.sig) == Some(k);
        if !first || scored.iter().any(|r| r.sig == p.sig) {
            continue;
        }
        let id = format!("r{next}");
        next += 1;
        added.push(id.clone());
        fresh.push(Rule {
            id,
            text: String::from(p.text.trim()),
            scope: p.scope,
            sig: p.sig.clone(),
            a: 2.0,
            b: 1.0,
        });
    }
    let all: Vec<Rule> = rewritten.into_iter().chain(fresh).collect();
    let mut kept: Vec<Rule> = all
        .iter()
        .filter(|r| !(weight(r) < PRUNE_BELOW && r.a + r.b >= 4.0))
        .cloned()
        .collect();
    by_weight_desc(&mut kept);
    kept.truncate(MAX_RULES);
    let pruned = all
        .iter()
        .filter(|r| !kept.iter().any(|k| k.id == r.id))
        .map(|r| r.id.clone())
        .collect();
    Learned {
        rules: kept,
        next_id: next,
        added,
        revised,
        pruned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn rule(id: &str, scope: Scope, sig: &str, a: f64, b: f64) -> Rule {
        Rule {
            id: String::from(id),
            text: format!("text {id}"),
            scope,
            sig: String::from(sig),
            a,
            b,
        }
    }

    fn ids(v: &[Rule]) -> Vec<&str> {
        v.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn prompts_get_strong_rules_for_their_scope_strongest_first() {
        let rules = [
            rule("r1", Scope::Worker, "s1", 3.0, 1.0),
            rule("r2", Scope::All, "s2", 1.0, 1.0),
            rule("r3", Scope::Worker, "s3", 1.0, 4.0),
            rule("r4", Scope::Planner, "s4", 2.0, 1.0),
        ];
        assert_eq!(ids(&for_prompt(&rules, Scope::Worker)), ["r1", "r2"]);
        assert_eq!(ids(&for_prompt(&rules, Scope::Planner)), ["r4", "r2"]);
        let many: Vec<Rule> = (0..12)
            .map(|i| rule(&format!("x{i}"), Scope::All, "s", 2.0, 1.0))
            .collect();
        assert_eq!(for_prompt(&many, Scope::Worker).len(), RULES_PER_PROMPT);
    }

    #[test]
    fn credit_held_and_recurred_only_for_rules_in_play() {
        let rules = [
            rule("r1", Scope::Worker, "bad", 3.0, 1.0),
            rule("r2", Scope::All, "other", 1.0, 1.0),
            rule("r3", Scope::All, "x", 1.0, 1.0),
        ];
        let inplay = vec![String::from("r1"), String::from("r2")];
        let got = credit(&rules, &inplay, &[String::from("bad")]);
        assert_eq!((got[0].a, got[0].b), (3.0, 2.0));
        assert_eq!((got[1].a, got[1].b), (1.25, 1.0));
        assert_eq!((got[2].a, got[2].b), (1.0, 1.0));
    }

    #[test]
    fn birth_rewrite_prune_and_cap() {
        let scored = [
            rule("r1", Scope::Worker, "known", 3.0, 2.0),
            rule("r3", Scope::Worker, "stale", 1.0, 4.0),
        ];
        let draft = |sig: &str| RuleDraft {
            sig: String::from(sig),
            text: format!(" new {sig} "),
            scope: Scope::All,
        };
        let got = apply(
            &scored,
            &[draft("known"), draft("fresh"), draft("fresh")],
            5,
        );
        assert_eq!(got.revised, ["r1"]);
        assert_eq!(got.added, ["r5"]);
        assert_eq!(got.pruned, ["r3"]);
        assert_eq!(got.next_id, 6);
        assert_eq!(ids(&got.rules), ["r5", "r1"]);
        assert_eq!(got.rules[0].text, "new fresh");
        assert_eq!((got.rules[0].a, got.rules[0].b), (2.0, 1.0));
        let many: Vec<Rule> = (0..35)
            .map(|i| rule(&format!("x{i}"), Scope::All, &format!("s{i}"), 2.0, 1.0))
            .collect();
        let capped = apply(&many, &[], 1);
        assert_eq!(capped.rules.len(), MAX_RULES);
        assert_eq!(capped.pruned.len(), 5);
    }
}
