//! Independent review against the Haruspex criteria, with one bounded fix cycle (design §6.4 step 6).
//! The same loop serves dev's `review` stage and every later re-review stage (Phase 4): the review
//! in progress (`RunState::reviewing`) names its labels, model and rounds.

use super::build::{fix_calls, summary};
use super::vars::{review_problem, review_vars};
use super::{answer_for, call, checks, learn, stages, CallSpec};
use crate::alloc_tiers::fix_tier;
use crate::api::{CheckOutcome, Command, Reply, Review, Role, SchemaId, Tier, UnitNote};
use crate::model::{Answer, Phase, ReviewCfg, ReviewFixRecord, RunState};
use crate::plan::blame::blame_review;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Dev's review stage: skipped when nothing was built or the run stopped.
pub fn start(s: &mut RunState, out: &mut Vec<Command>) {
    if s.stopped || s.built.is_empty() {
        learn::start(s, out);
        return;
    }
    let p = s.pipeline();
    s.reviewing = Some(ReviewCfg {
        prefix: String::from("review"),
        fix_prefix: String::from("fix"),
        llm: p.review.clone(),
        max_fix_rounds: p.review_max_fix_rounds,
    });
    request(s, 1, out);
}

fn cfg(s: &RunState) -> ReviewCfg {
    s.reviewing
        .clone()
        .expect("a review answer follows a review request")
}

/// Asks for review number `n` of the review in progress.
pub fn request(s: &mut RunState, n: u32, out: &mut Vec<Command>) {
    let c = cfg(s);
    let vars = review_vars(s, n);
    s.phase = Phase::Reviewing { round: n };
    call(
        s,
        CallSpec {
            label: format!("{}#{n}", c.prefix),
            role: Role::Reviewer,
            unit: None,
            tier: Tier::Opus,
            effort: c.llm.effort,
            template: &c.llm.prompt,
            vars,
            schema: SchemaId::Review,
            max_turns: c.llm.max_turns,
            timeout_secs: c.llm.timeout_secs,
        },
        out,
    );
}

/// `n` is the review number: review#1, then review#2 after the first fix round.
pub fn on_review(s: &mut RunState, n: u32, a: &Answer, out: &mut Vec<Command>) {
    match a {
        Answer::Reply(Reply::Review(r), _) => s.review = Some(r.clone()),
        _ if n == 1 => {
            out.push(Command::Log(String::from(
                "the reviewer died; the change is not independently verified",
            )));
            s.review = None;
        }
        // A re-reviewer that dies leaves the earlier verdict standing.
        _ => {}
    }
    let c = cfg(s);
    let max = c.max_fix_rounds;
    let rejected = s.review.as_ref().is_some_and(|r| !r.passed);
    if rejected && n <= max {
        let review = s.review.clone().expect("rejected implies a review");
        let culprits = blame_review(&review, &s.built);
        let who: Vec<String> = culprits
            .iter()
            .map(|u| format!("{} on {}", u.id, fix_tier(u.tier, n).name()))
            .collect();
        out.push(Command::Log(format!(
            "review rejected the change; fix {n}/{max} for {}",
            who.join(", ")
        )));
        s.layer.culprits = culprits.clone();
        s.phase = Phase::ReviewFixing { round: n };
        let fp = c.fix_prefix.clone();
        fix_calls(
            s,
            &culprits,
            &review_problem(&review),
            (n, max),
            false,
            &|u| format!("{fp}:{u}#r{n}"),
            out,
        );
        return;
    }
    finish(s, out);
}

pub fn on_fix(s: &mut RunState, round: u32, answers: &[(String, Answer)], out: &mut Vec<Command>) {
    let review = s.review.clone().expect("a review fix follows a rejection");
    let c = cfg(s);
    let culprits = s.layer.culprits.clone();
    let summaries = culprits
        .iter()
        .map(|u| {
            summary(answer_for(
                answers,
                &format!("{}:{}#r{round}", c.fix_prefix, u.id),
            ))
        })
        .collect();
    s.review_fixes.push(ReviewFixRecord {
        round,
        units: culprits.iter().map(|u| u.id.clone()).collect(),
        findings: review
            .findings
            .iter()
            .filter(|f| f.severity.serious())
            .cloned()
            .collect(),
        failed_criteria: review
            .criteria
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.id.clone())
            .collect(),
        fix_summaries: summaries,
    });
    s.phase = Phase::ReviewChecking { round };
    let gate = s.gate.clone();
    checks(s, format!("check:{}{round}", c.prefix), gate, out);
}

pub fn on_recheck(
    s: &mut RunState,
    round: u32,
    results: &[CheckOutcome],
    changed: &[String],
    out: &mut Vec<Command>,
) {
    s.changed = changed.to_vec();
    let failed: Vec<&str> = results
        .iter()
        .filter(|r| r.exit_code != 0)
        .map(|r| r.cmd.as_str())
        .collect();
    if !failed.is_empty() {
        out.push(Command::Log(format!(
            "checks failed after the review fix: {}",
            failed.join(" | ")
        )));
        s.review = s.review.clone().map(|r| Review { passed: false, ..r });
        finish(s, out);
        return;
    }
    request(s, round + 1, out);
}

/// A review that still rejects leaves its culprits stuck; then learn (dev) or the next stage.
fn finish(s: &mut RunState, out: &mut Vec<Command>) {
    let culprits: Vec<UnitNote> = match s.review.clone().filter(|r| !r.passed) {
        Some(r) => blame_review(&r, &s.built)
            .iter()
            .map(|u| UnitNote {
                id: u.id.clone(),
                goal: u.goal.clone(),
                why: String::from("independent review still rejects it"),
            })
            .collect(),
        None => Vec::new(),
    };
    s.reviewing = None;
    if s.stage_at.is_some() {
        stages::after_review(s, culprits, out);
        return;
    }
    s.stuck.extend(culprits);
    learn::start(s, out);
}
