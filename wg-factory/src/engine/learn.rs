//! Learn: rule credit, the learn call, rule birth and pruning, model stats, `SaveMemory` and
//! `Finish` (design §6.4 step 7).

use super::call;
use super::vars::{json_str, json_strs, var};
use super::CallSpec;
use crate::api::{
    Command, LlmFailure, Memory, Outcome, Reply, Role, RuleDraft, RulesReport, SchemaId, Status,
};
use crate::learn::models::update;
use crate::learn::rules::{apply, credit};
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
            l.rules
                .iter()
                .filter(|p| s.fixed_sigs.contains(&p.sig) && !p.text.trim().is_empty())
                .cloned()
                .collect()
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
    let scored = credit(&s.memory.rules, &s.in_play, &recurred(s));
    let learned = apply(&scored, phrased, s.memory.next_id);
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
            rules: learned.rules.clone(),
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
        top: learned.rules.iter().take(5).cloned().collect(),
    };
    super::publish::finish_or_publish(s, o, out);
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
        rules: RulesReport::default(),
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
