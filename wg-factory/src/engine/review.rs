//! Independent review against the Haruspex criteria, with one bounded fix cycle (design §6.4 step 6).

use super::build::summary;
use super::vars::{fix_vars, review_problem, review_vars};
use super::{answer_for, call, checks, learn, CallSpec};
use crate::alloc_tiers::fix_tier;
use crate::api::{CheckOutcome, Command, Reply, Review, Role, SchemaId, Tier, UnitNote};
use crate::model::{Answer, Phase, ReviewFixRecord, RunState};
use crate::plan::blame::blame_review;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub fn start(s: &mut RunState, out: &mut Vec<Command>) {
    if s.stopped || s.built.is_empty() {
        learn::start(s, out);
        return;
    }
    request_review(s, 1, out);
}

fn request_review(s: &mut RunState, n: u32, out: &mut Vec<Command>) {
    let stage = s.pipeline().review.clone();
    let vars = review_vars(s, n);
    s.phase = Phase::Reviewing { round: n };
    call(
        s,
        CallSpec {
            label: format!("review#{n}"),
            role: Role::Reviewer,
            unit: None,
            tier: Tier::Opus,
            effort: stage.effort,
            template: &stage.prompt,
            vars,
            schema: SchemaId::Review,
            max_turns: stage.max_turns,
            timeout_secs: stage.timeout_secs,
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
    let max = s.pipeline().review_max_fix_rounds;
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
        let problem = review_problem(&review);
        let pipeline = s.pipeline().clone();
        s.layer.culprits = culprits.clone();
        s.phase = Phase::ReviewFixing { round: n };
        for u in &culprits {
            let vars = fix_vars(s, u, &problem, n, max);
            call(
                s,
                CallSpec {
                    label: format!("fix:{}#r{n}", u.id),
                    role: Role::Fixer,
                    unit: Some(&u.id),
                    tier: fix_tier(u.tier, n),
                    effort: None,
                    template: &pipeline.fix_prompt,
                    vars,
                    schema: SchemaId::Work,
                    max_turns: pipeline.build_max_turns,
                    timeout_secs: pipeline.build_timeout_secs,
                },
                out,
            );
        }
        return;
    }
    finish(s, out);
}

pub fn on_fix(s: &mut RunState, round: u32, answers: &[(String, Answer)], out: &mut Vec<Command>) {
    let review = s.review.clone().expect("a review fix follows a rejection");
    let culprits = s.layer.culprits.clone();
    let summaries = culprits
        .iter()
        .map(|u| summary(answer_for(answers, &format!("fix:{}#r{round}", u.id))))
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
    checks(s, format!("check:review{round}"), gate, out);
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
    request_review(s, round + 1, out);
}

/// A review that still rejects leaves its culprits stuck; then learn.
fn finish(s: &mut RunState, out: &mut Vec<Command>) {
    if let Some(r) = s.review.clone().filter(|r| !r.passed) {
        for u in blame_review(&r, &s.built) {
            s.stuck.push(UnitNote {
                id: u.id.clone(),
                goal: u.goal.clone(),
                why: String::from("independent review still rejects it"),
            });
        }
    }
    learn::start(s, out);
}
