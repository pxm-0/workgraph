//! The stages after dev (design §6.1, Phase 4): QA check sets and agents with an optional blamed
//! fix round through the build fix loop, re-reviews, sandbox steps, Haruspex gates, the human stage
//! and publish. Each stage runs in chain order; the first that fails leaves the run stuck.

use super::build::{fix_calls, record_fixes};
use super::vars::{agent_vars, check_problem, verdict_problem};
use super::{call, checks, review, CallSpec};
use crate::api::{CheckOutcome, Command, OnFail, Outcome, Review, StageKind, Status, UnitNote};
use crate::model::{Answer, LayerState, Phase, ReviewCfg, RunState};
use crate::plan::blame::{blame, blame_review};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// After dev (or at once, for a chain without dev): a run ready for review goes through the later
/// stages; any other outcome finishes as it is.
pub fn begin(s: &mut RunState, o: Outcome, out: &mut Vec<Command>) {
    if o.status != Status::ReadyForReview || s.stages.is_empty() {
        s.phase = Phase::Done;
        out.push(Command::Finish(o));
        return;
    }
    s.finished = Some(o);
    s.layer = LayerState {
        ok: s.built.clone(),
        ..LayerState::default()
    };
    next(s, 0, out);
}

fn label(s: &RunState) -> String {
    s.stages[s.stage_at.expect("a stage is running")]
        .label
        .clone()
}

fn kind(s: &RunState) -> StageKind {
    s.stages[s.stage_at.expect("a stage is running")]
        .kind
        .clone()
}

/// Starts the first stage at or after `from` that has work to do; finishes after the last one.
pub fn next(s: &mut RunState, from: usize, out: &mut Vec<Command>) {
    for i in from..s.stages.len() {
        s.stage_at = Some(i);
        let p = s.stages[i].label.clone();
        // Dev built nothing: there is nothing to check, explore or review (publish still asks,
        // and the kernel refuses it without tokens).
        let idle = s.pipeline.is_some() && s.built.is_empty();
        match s.stages[i].kind.clone() {
            StageKind::Checks { names, .. } if !idle => {
                s.phase = Phase::StageChecking { round: 0 };
                checks(s, format!("check:{p}"), names, out);
            }
            StageKind::Agent { .. } if !idle => agent(s, 1, String::new(), out),
            StageKind::Review { .. } if idle || !s.dirty_since_review => {
                out.push(Command::Log(format!(
                    "{}: no change since the last review, so no re-review",
                    s.stages[i].id
                )));
                continue;
            }
            StageKind::Review {
                llm,
                max_fix_rounds,
            } => {
                s.reviewing = Some(ReviewCfg {
                    prefix: p.clone(),
                    fix_prefix: p,
                    llm,
                    max_fix_rounds,
                });
                review::request(s, 1, out);
            }
            StageKind::Sandbox { always, .. } if !always && !s.risk.tainted => continue,
            StageKind::Sandbox { steps_from, .. } => {
                s.phase = Phase::Sandboxing;
                s.pending.push(p.clone());
                out.push(Command::Sandbox {
                    label: p,
                    steps_from,
                });
            }
            StageKind::HaruspexGate { gate, .. } => {
                s.phase = Phase::Gating;
                s.pending.push(p.clone());
                out.push(Command::Gate { label: p, gate });
            }
            StageKind::Human { gate, ask } => {
                s.phase = Phase::Done;
                out.push(Command::Human { gate, ask });
            }
            StageKind::Publish(_) => {
                s.phase = Phase::Publishing;
                out.push(Command::Publish {
                    title: super::publish::title(&s.title),
                });
            }
            _ => continue,
        }
        return;
    }
    s.phase = Phase::Done;
    let o = settled(s, None, Vec::new());
    out.push(Command::Finish(o));
}

/// The held outcome with the chain's latest review and fix counts; a failure makes it stuck.
pub fn settled(s: &mut RunState, failure: Option<String>, stuck: Vec<UnitNote>) -> Outcome {
    let o = s
        .finished
        .take()
        .unwrap_or_else(|| super::learn::failed(s, String::new()));
    let mut all_stuck = o.stuck.clone();
    all_stuck.extend(stuck);
    Outcome {
        status: if failure.is_some() {
            Status::Stuck
        } else {
            o.status
        },
        note: failure.or(o.note.clone()),
        review: s.review.clone().or(o.review.clone()),
        stuck: all_stuck,
        fix_rounds: s.fixes.len() as u32,
        review_fix_rounds: s.review_fixes.len() as u32,
        ..o
    }
}

/// Ends the run stuck at the current stage.
fn fail(s: &mut RunState, why: String, stuck: Vec<UnitNote>, out: &mut Vec<Command>) {
    let id = s.stages[s.stage_at.expect("a stage is running")].id.clone();
    s.phase = Phase::Done;
    let o = settled(s, Some(format!("stage {id}: {why}")), stuck);
    out.push(Command::Finish(o));
}

fn notes(units: &[crate::model::Unit], why: &str) -> Vec<UnitNote> {
    units
        .iter()
        .map(|u| UnitNote {
            id: u.id.clone(),
            goal: u.goal.clone(),
            why: String::from(why),
        })
        .collect()
}

fn failed_cmds(results: &[CheckOutcome]) -> Vec<CheckOutcome> {
    results
        .iter()
        .filter(|r| r.exit_code != 0)
        .cloned()
        .collect()
}

/// A `checks` stage's results: pass on, or fix the blamed units and check again, or fail.
pub fn on_checks(
    s: &mut RunState,
    round: u32,
    results: &[CheckOutcome],
    changed: &[String],
    out: &mut Vec<Command>,
) {
    s.changed = changed.to_vec();
    let StageKind::Checks {
        on_fail,
        max_fix_rounds,
        ..
    } = kind(s)
    else {
        return;
    };
    let failed = failed_cmds(results);
    let i = s.stage_at.expect("a stage is running");
    if failed.is_empty() {
        for sig in s.layer.sigs.clone() {
            crate::model::add(&mut s.fixed_sigs, &sig);
        }
        next(s, i + 1, out);
        return;
    }
    let cmds: Vec<&str> = failed.iter().map(|f| f.cmd.as_str()).collect();
    let why = format!("checks failing: {}", cmds.join(" | "));
    if on_fail == OnFail::Stop || round >= max_fix_rounds {
        let culprits = blame(&failed, &s.built);
        fail(s, why, notes(&culprits, "QA checks still fail"), out);
        return;
    }
    let round = round + 1;
    let culprits = blame(&failed, &s.built);
    out.push(Command::Log(format!(
        "{}: {why}; fix {round}/{max_fix_rounds}",
        s.stages[i].id
    )));
    s.phase = Phase::StageFixing { round };
    s.layer.culprits = culprits.clone();
    s.layer.failed = failed.clone();
    s.dirty_since_review = true;
    let p = label(s);
    fix_calls(
        s,
        &culprits,
        &check_problem(&failed),
        (round, max_fix_rounds),
        true,
        &|u| format!("{p}:{u}#{round}"),
        out,
    );
}

pub fn on_checks_fixed(
    s: &mut RunState,
    round: u32,
    answers: &[(String, Answer)],
    out: &mut Vec<Command>,
) {
    let (p, id) = (label(s), s.stages[s.stage_at.expect("running")].id.clone());
    record_fixes(s, id, round, answers, &|u| format!("{p}:{u}#{round}"));
    let StageKind::Checks { names, .. } = kind(s) else {
        return;
    };
    s.phase = Phase::StageChecking { round };
    checks(s, format!("check:{p}#{round}"), names, out);
}

/// Calls an `agent` stage: run 1, or run 2 after its fix round with the earlier findings.
fn agent(s: &mut RunState, run: u32, problem: String, out: &mut Vec<Command>) {
    let StageKind::Agent {
        role, llm, schema, ..
    } = kind(s)
    else {
        return;
    };
    let p = label(s);
    let vars = agent_vars(s, &problem, run);
    s.phase = Phase::Agent { run };
    call(
        s,
        CallSpec {
            label: if run == 1 { p } else { format!("{p}#{run}") },
            role,
            unit: None,
            tier: llm.tier,
            effort: llm.effort,
            template: &llm.prompt,
            vars,
            schema,
            max_turns: llm.max_turns,
            timeout_secs: llm.timeout_secs,
        },
        out,
    );
}

pub fn on_agent(s: &mut RunState, run: u32, a: &Answer, out: &mut Vec<Command>) {
    let StageKind::Agent { gate, on_fail, .. } = kind(s) else {
        return;
    };
    let reply = match a {
        Answer::Reply(r, _) => r.clone(),
        Answer::Failed(why) => {
            fail(s, format!("the agent failed ({why:?})"), Vec::new(), out);
            return;
        }
    };
    let passed = gate
        .as_ref()
        .is_none_or(|g| reply.flag(&g.field) == Some(g.equals));
    let i = s.stage_at.expect("a stage is running");
    if passed {
        next(s, i + 1, out);
        return;
    }
    let g = gate.expect("only a gate can fail");
    let why = format!("its gate {} == {} does not hold", g.field, g.equals);
    let findings = reply.findings().to_vec();
    let culprits = blame_review(
        &Review {
            passed: false,
            criteria: Vec::new(),
            findings: findings.clone(),
        },
        &s.built,
    );
    if on_fail == OnFail::Stop || run > 1 || culprits.is_empty() {
        fail(s, why, notes(&culprits, "QA still finds it wrong"), out);
        return;
    }
    out.push(Command::Log(format!(
        "{}: {why}; one fix round",
        s.stages[i].id
    )));
    s.phase = Phase::AgentFixing;
    s.layer.culprits = culprits.clone();
    s.layer.failed = Vec::new();
    s.dirty_since_review = true;
    s.agent_problem = verdict_problem(&findings);
    let (p, problem) = (label(s), s.agent_problem.clone());
    fix_calls(
        s,
        &culprits,
        &problem,
        (1, 1),
        false,
        &|u| format!("{p}:{u}#1"),
        out,
    );
}

pub fn on_agent_fixed(s: &mut RunState, answers: &[(String, Answer)], out: &mut Vec<Command>) {
    let (p, id) = (label(s), s.stages[s.stage_at.expect("running")].id.clone());
    record_fixes(s, id, 1, answers, &|u| format!("{p}:{u}#1"));
    s.phase = Phase::AgentChecking;
    checks(s, format!("check:{p}#1"), Vec::new(), out);
}

pub fn on_agent_recheck(s: &mut RunState, results: &[CheckOutcome], out: &mut Vec<Command>) {
    let failed = failed_cmds(results);
    if !failed.is_empty() {
        let cmds: Vec<&str> = failed.iter().map(|f| f.cmd.as_str()).collect();
        let culprits = s.layer.culprits.clone();
        let why = format!("checks fail after its fix round: {}", cmds.join(" | "));
        fail(s, why, notes(&culprits, "its QA fix broke a check"), out);
        return;
    }
    let problem = s.agent_problem.clone();
    agent(s, 2, problem, out);
}

/// A re-review that passed clears the change for the next stage; one that rejects or died fails.
pub fn after_review(s: &mut RunState, stuck: Vec<UnitNote>, out: &mut Vec<Command>) {
    if !s.review.as_ref().is_some_and(|r| r.passed) {
        fail(s, String::from("the re-review did not pass"), stuck, out);
        return;
    }
    s.dirty_since_review = false;
    next(s, s.stage_at.expect("a stage is running") + 1, out);
}

pub fn on_sandbox(
    s: &mut RunState,
    label_: &str,
    results: &[CheckOutcome],
    out: &mut Vec<Command>,
) {
    if !take(s, label_, Phase::Sandboxing, out) {
        return;
    }
    let failed = failed_cmds(results);
    if !failed.is_empty() {
        let steps: Vec<String> = failed
            .iter()
            .map(|f| format!("{} (exit {})", f.name, f.exit_code))
            .collect();
        fail(
            s,
            format!("sandbox steps failed: {}", steps.join(", ")),
            Vec::new(),
            out,
        );
        return;
    }
    next(s, s.stage_at.expect("a stage is running") + 1, out);
}

pub fn on_gate(s: &mut RunState, label_: &str, blockers: &[String], out: &mut Vec<Command>) {
    if !take(s, label_, Phase::Gating, out) {
        return;
    }
    let StageKind::HaruspexGate { gate, expect_only } = kind(s) else {
        return;
    };
    let unexpected: Vec<&str> = blockers
        .iter()
        .filter(|b| !expect_only.contains(b))
        .map(String::as_str)
        .collect();
    if !unexpected.is_empty() {
        fail(
            s,
            format!("{gate} has other blockers: {}", unexpected.join(" | ")),
            Vec::new(),
            out,
        );
        return;
    }
    next(s, s.stage_at.expect("a stage is running") + 1, out);
}

/// Whether `label_` answers the outstanding stage command.
fn take(s: &mut RunState, label_: &str, phase: Phase, out: &mut Vec<Command>) -> bool {
    match s.pending.iter().position(|p| p == label_) {
        Some(i) if s.phase == phase => {
            s.pending.remove(i);
            true
        }
        _ => {
            out.push(Command::Log(format!(
                "ignored a result for {label_}, which was not requested"
            )));
            false
        }
    }
}
