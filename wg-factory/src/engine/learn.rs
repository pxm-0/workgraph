//! Learn: rule credit, the learn call, rule birth and pruning, model stats, `SaveMemory` and
//! `Finish` (design §6.4 step 7).

use super::call;
use super::vars::{json_str, json_strs, var};
use super::CallSpec;
use crate::api::{
    Command, LlmFailure, Memory, Outcome, Proposal, Reply, Role, RuleDraft, RulesReport, SchemaId,
    Status,
};
use crate::learn::models::update;
use crate::learn::rules::{apply, credit, retire, stopgaps, with_checks};
use crate::model::{Answer, Phase, RunState};
use crate::plan::scope::out_of_scope;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

fn initialized(s: &RunState) -> bool {
    s.reading.as_ref().is_some_and(|r| r.initialized) || s.initialized_earlier
}

fn scope_slips(s: &RunState) -> Vec<String> {
    out_of_scope(
        &s.changed,
        &s.units,
        &s.user_changes_before,
        &s.preexisting,
        initialized(s),
    )
}

fn recurred(s: &RunState) -> Vec<String> {
    s.fixes
        .iter()
        .flat_map(|x| x.failed.iter().map(|f| f.sig.clone()))
        .collect()
}

/// The evidence the learn call turns into rules and proposals, as a JSON array.
fn evidence(s: &RunState, out_of_scope: &[String]) -> Vec<String> {
    let scored = credit(&s.memory.rules, &s.in_play, &recurred(s));
    let mut items = Vec::new();
    for sig in &s.fixed_sigs {
        let Some(x) = s
            .fixes
            .iter()
            .find(|fx| fx.failed.iter().any(|f| &f.sig == sig))
        else {
            continue;
        };
        let f = x
            .failed
            .iter()
            .find(|f| &f.sig == sig)
            .expect("found above");
        let existing = scored
            .iter()
            .find(|r| &r.sig == sig)
            .map_or(String::from("null"), |r| json_str(&r.text));
        items.push(format!(
            "{{\"sig\":{},\"cmd\":{},\"outputTail\":{},\"fixSummaries\":{},\"existingRule\":{}}}",
            json_str(sig),
            json_str(&f.cmd),
            json_str(&f.output_tail),
            json_strs(&x.fix_summaries),
            existing
        ));
    }
    for r in &s.review_fixes {
        let findings: Vec<String> = r
            .findings
            .iter()
            .map(|f| {
                format!(
                    "{{\"id\":{},\"severity\":{},\"file\":{},\"summary\":{}}}",
                    json_str(&f.id),
                    json_str(f.severity.name()),
                    f.file.as_deref().map_or(String::from("null"), json_str),
                    json_str(&f.summary)
                )
            })
            .collect();
        items.push(format!(
            "{{\"reviewFindings\":[{}],\"failedCriteria\":{},\"fixSummaries\":{}}}",
            findings.join(","),
            json_strs(&r.failed_criteria),
            json_strs(&r.fix_summaries)
        ));
    }
    if !out_of_scope.is_empty() {
        items.push(format!("{{\"outOfScope\":{}}}", json_strs(out_of_scope)));
    }
    for st in &s.stuck {
        items.push(format!(
            "{{\"stuck\":{{\"id\":{},\"goal\":{},\"why\":{}}}}}",
            json_str(&st.id),
            json_str(&st.goal),
            json_str(&st.why)
        ));
    }
    items
}

pub fn start(s: &mut RunState, out: &mut Vec<Command>) {
    let slips = scope_slips(s);
    if !slips.is_empty() {
        out.push(Command::Log(format!(
            "changed outside the plan: {}",
            slips.join(", ")
        )));
    }
    let items = evidence(s, &slips);
    if items.is_empty() || !s.can_save {
        finish(s, &[], out);
        return;
    }
    let stage = s.pipeline().learn.clone();
    s.phase = Phase::Learning;
    call(
        s,
        CallSpec {
            label: String::from("learn"),
            role: Role::Learner,
            unit: None,
            tier: stage.tier,
            effort: stage.effort,
            template: &stage.prompt,
            vars: alloc::vec![var("evidence", format!("[{}]", items.join(",")))],
            schema: SchemaId::Learn,
            max_turns: stage.max_turns,
            timeout_secs: stage.timeout_secs,
        },
        out,
    );
}

pub fn on_learn(s: &mut RunState, a: &Answer, out: &mut Vec<Command>) {
    let phrased: Vec<RuleDraft> = match a {
        Answer::Reply(Reply::Learn(l), _) => {
            s.proposals = l.proposals.clone();
            let kept: Vec<RuleDraft> = l
                .rules
                .iter()
                .filter(|p| s.fixed_sigs.contains(&p.sig) && !p.text.trim().is_empty())
                .cloned()
                .collect();
            // A check this run already had let the failure through, so it cannot be the fix;
            // linking it would retire the rule before the real check exists.
            let linkable: Vec<Proposal> = l
                .proposals
                .iter()
                .filter(|p| match &p.check_name {
                    Some(n) if s.check_cmds.iter().any(|(name, _)| name == n) => {
                        out.push(Command::Log(format!(
                            "check proposal names the existing check {n:?}; its rule stays a plain rule"
                        )));
                        false
                    }
                    _ => true,
                })
                .cloned()
                .collect();
            with_checks(&kept, &linkable)
        }
        Answer::Failed(why) => {
            let what = if *why == LlmFailure::Invalid {
                "returned invalid output twice"
            } else {
                "died"
            };
            out.push(Command::Log(format!(
                "learn agent {what}; no new rules or proposals this run"
            )));
            Vec::new()
        }
        Answer::Reply(..) => Vec::new(),
    };
    finish(s, &phrased, out);
}

fn finish(s: &mut RunState, phrased: &[RuleDraft], out: &mut Vec<Command>) {
    // Only a check that gates this run enforces anything; retiring before `apply` keeps a retired
    // stopgap from taking a `MAX_RULES` slot from a rule that still earns it.
    let (scored, retired) = retire(&credit(&s.memory.rules, &s.in_play, &recurred(s)), &s.gate);
    let learned = apply(&scored, phrased, s.memory.next_id);
    let rules = learned.rules;
    let models = update(&s.memory.models, &s.outcomes);
    let checks: Vec<String> = if s.gate.is_empty() {
        s.memory.checks.clone()
    } else {
        s.gate
            .iter()
            .filter_map(|n| s.check_cmds.iter().find(|(name, _)| name == n))
            .map(|(_, cmd)| cmd.clone())
            .collect()
    };
    if s.can_save {
        out.push(Command::SaveMemory(Memory {
            runs: s.memory.runs + 1,
            next_id: learned.next_id,
            checks,
            rules: rules.clone(),
            models,
        }));
    }
    let mut o = base_outcome(s);
    o.status = if s.stuck.is_empty() {
        Status::ReadyForReview
    } else {
        Status::Stuck
    };
    o.out_of_scope = scope_slips(s);
    o.rules = RulesReport {
        added: learned.added,
        revised: learned.revised,
        pruned: learned.pruned,
        in_play: s.in_play.clone(),
        top: rules.iter().take(5).cloned().collect(),
        stopgaps: stopgaps(&rules),
        retired,
    };
    super::stages::begin(s, o, out);
}

fn base_outcome(s: &RunState) -> Outcome {
    Outcome {
        status: Status::Failed,
        note: None,
        review: s.review.clone(),
        done: s.done.clone(),
        skipped: s.skipped.clone(),
        stuck: s.stuck.clone(),
        fix_rounds: s.fixes.len() as u32,
        review_fix_rounds: s.review_fixes.len() as u32,
        out_of_scope: Vec::new(),
        committable: s.committable.clone(),
        models: s.allocation.clone(),
        rules: RulesReport {
            stopgaps: stopgaps(&s.memory.rules),
            ..RulesReport::default()
        },
        proposals: s.proposals.clone(),
    }
}

/// An early end: `Failed` with a note (callers may change the status).
pub fn failed(s: &RunState, note: String) -> Outcome {
    Outcome {
        note: Some(note),
        ..base_outcome(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Learn, Rule, Scope};
    use alloc::vec;

    fn rule(id: &str, sig: &str, check_name: Option<&str>) -> Rule {
        Rule {
            id: String::from(id),
            text: format!("rule {id}"),
            scope: Scope::Worker,
            sig: String::from(sig),
            a: 3.0,
            b: 1.0,
            check: check_name.map(|_| String::from("a lint")),
            check_name: check_name.map(String::from),
        }
    }

    fn state(rules: Vec<Rule>, gate: &[&str], failing: &[&str]) -> RunState {
        let names = gate.iter().chain(failing);
        RunState {
            can_save: true,
            check_cmds: names
                .map(|n| (String::from(*n), format!("run {n}")))
                .collect(),
            gate: gate.iter().map(|n| String::from(*n)).collect(),
            fixed_sigs: vec![String::from("new")],
            memory: Memory {
                next_id: rules.len() as u64 + 1,
                rules,
                ..Memory::default()
            },
            ..RunState::default()
        }
    }

    fn learn(check_name: &str) -> Answer {
        Answer::Reply(
            Reply::Learn(Learn {
                rules: vec![RuleDraft {
                    sig: String::from("new"),
                    text: String::from("do it right"),
                    scope: Scope::Worker,
                    check: None,
                    check_name: None,
                }],
                proposals: vec![Proposal {
                    category: String::from("automated-check"),
                    proposal: String::from("an eslint rule"),
                    evidence: String::from("e"),
                    sig: Some(String::from("new")),
                    check_name: Some(String::from(check_name)),
                }],
            }),
            true,
        )
    }

    fn run(s: &mut RunState, a: &Answer) -> (Vec<Rule>, Outcome, Vec<Command>) {
        let mut out = Vec::new();
        on_learn(s, a, &mut out);
        let saved = out.iter().find_map(|c| match c {
            Command::SaveMemory(m) => Some(m.rules.clone()),
            _ => None,
        });
        let done = out.iter().find_map(|c| match c {
            Command::Finish(o) => Some(o.clone()),
            _ => None,
        });
        (saved.unwrap(), done.unwrap(), out)
    }

    #[test]
    fn a_new_check_name_makes_a_saved_stopgap_and_an_existing_one_does_not() {
        let mut s = state(vec![], &["lint"], &[]);
        let (saved, o, _) = run(&mut s, &learn("no-effect"));
        assert_eq!(saved[0].check_name.as_deref(), Some("no-effect"));
        assert_eq!(o.rules.stopgaps.len(), 1);
        assert!(o.rules.retired.is_empty());

        let mut s = state(vec![], &["lint"], &[]);
        let (saved, o, out) = run(&mut s, &learn("lint"));
        assert_eq!(
            saved.len(),
            1,
            "the lesson is kept, not born and retired at once"
        );
        assert_eq!(saved[0].check_name, None);
        assert!(o.rules.retired.is_empty() && o.rules.stopgaps.is_empty());
        assert!(out
            .iter()
            .any(|c| matches!(c, Command::Log(m) if m.contains("existing check"))));
    }

    #[test]
    fn a_gating_check_retires_its_stopgap_before_the_cap_and_a_failing_one_does_not() {
        let mut rules: Vec<Rule> = (1..30)
            .map(|i| rule(&format!("r{i}"), &format!("s{i}"), None))
            .collect();
        rules.push(rule("r30", "s30", Some("no-effect")));
        let mut s = state(rules.clone(), &["no-effect"], &[]);
        let (saved, o, _) = run(&mut s, &learn("other"));
        assert_eq!(
            o.rules
                .retired
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["r30"]
        );
        assert!(
            o.rules.pruned.is_empty(),
            "the retired slot makes room for the new rule"
        );
        assert_eq!(saved.len(), 30);
        assert!(saved.iter().any(|r| r.sig == "new"));

        let mut s = state(rules, &[], &["no-effect"]);
        let (saved, o, _) = run(&mut s, &learn("other"));
        assert!(
            o.rules.retired.is_empty(),
            "a check failing at baseline enforces nothing"
        );
        assert!(saved.iter().any(|r| r.id == "r30"));
    }

    #[test]
    fn an_early_end_still_reports_the_pending_stopgaps() {
        let s = state(
            vec![rule("r1", "s1", Some("no-effect")), rule("r2", "s2", None)],
            &[],
            &[],
        );
        let o = failed(&s, String::from("stopped"));
        assert_eq!(
            o.rules
                .stopgaps
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["r1"]
        );
    }
}
