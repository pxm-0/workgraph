//! Baseline, Kahn layers, work, layer checks and blamed fix rounds (design §6.4 steps 4-5).

use super::vars::{check_problem, fix_vars, last_chars, work_vars};
use super::{answer_for, call, checks, review, CallSpec};
use crate::alloc_tiers::{fix_tier, pick_tier, stat_key};
use crate::api::{Allocation, CheckOutcome, Command, Effort, Reply, Role, SchemaId, UnitNote};
use crate::model::{
    add, Answer, FailedCheck, FixRecord, LayerState, ModelOutcome, Phase, RunState, Unit,
};
use crate::plan::blame::blame;
use crate::plan::layers::layers_of;
use crate::plan::signature::signature;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Every check result arrives here; the phase says which batch it answers.
pub fn on_checks(
    s: &mut RunState,
    results: &[CheckOutcome],
    changed: &[String],
    out: &mut Vec<Command>,
) {
    match s.phase.clone() {
        Phase::Baseline => on_baseline(s, results, changed, out),
        Phase::Checking { layer, round } => on_layer_check(s, layer, round, results, changed, out),
        Phase::ReviewChecking { round } => review::on_recheck(s, round, results, changed, out),
        Phase::StageChecking { round } => super::stages::on_checks(s, round, results, changed, out),
        Phase::AgentChecking => super::stages::on_agent_recheck(s, results, out),
        other => out.push(Command::Log(format!(
            "ignored check results in phase {other:?}"
        ))),
    }
}

fn on_baseline(
    s: &mut RunState,
    results: &[CheckOutcome],
    changed: &[String],
    out: &mut Vec<Command>,
) {
    s.left_out = results
        .iter()
        .filter(|r| r.exit_code != 0)
        .map(|r| r.name.clone())
        .collect();
    s.gate = s
        .check_cmds
        .iter()
        .map(|(n, _)| n.clone())
        .filter(|n| !s.left_out.contains(n))
        .collect();
    s.preexisting = changed.to_vec();
    s.changed = changed.to_vec();
    if !s.left_out.is_empty() {
        out.push(Command::Log(format!(
            "already failing before any change, left out of the gate: {}",
            s.left_out.join(" | ")
        )));
    }
    if s.gate.is_empty() {
        out.push(Command::Log(String::from(
            "no passing automated check here: units run once and the result is unverified",
        )));
    }
    let (layers, cycle) = layers_of(&s.units);
    if cycle {
        out.push(Command::Log(String::from(
            "dependency cycle among the remaining units; running them one at a time",
        )));
    }
    s.layers = layers;
    start_layer(s, 0, out);
}

/// Starts the first layer at or after `from` that has a runnable unit; else moves to review.
pub fn start_layer(s: &mut RunState, from: usize, out: &mut Vec<Command>) {
    for i in from..s.layers.len() {
        let mut runnable = Vec::new();
        for id in s.layers[i].clone() {
            let u = s.unit(&id).expect("layers hold known units").clone();
            let kernel_risky = s.risk.risky_units.iter().find(|(r, _)| *r == u.id);
            let dep = u.depends_on.iter().find(|d| s.blocked.contains(d)).cloned();
            let why = if s.stopped {
                Some(String::from(
                    "run stopped after an earlier layer failed its checks",
                ))
            } else if (u.risky || kernel_risky.is_some()) && !s.risk.sandbox_ready {
                // Phase 3: a ready sandbox runs it (the kernel applies the sandbox-only policy).
                let reason = u
                    .risk_reason
                    .clone()
                    .or_else(|| kernel_risky.map(|(_, r)| r.clone()))
                    .unwrap_or_else(|| String::from("hard to undo"));
                Some(format!("risky, sandbox unavailable, yours to do: {reason}"))
            } else {
                dep.map(|d| format!("depends on {d}, which did not finish"))
            };
            match why {
                Some(why) => {
                    add(&mut s.blocked, &u.id);
                    s.skipped.push(UnitNote {
                        id: u.id.clone(),
                        goal: u.goal.clone(),
                        why,
                    });
                }
                None => runnable.push(u),
            }
        }
        if runnable.is_empty() {
            continue;
        }
        let pipeline = s.pipeline().clone();
        let mut assigned = Vec::new();
        for u in runnable {
            let (tier, probed) = pick_tier(&s.memory, &stat_key(u.size), &s.probed);
            s.probed = probed;
            s.allocation.push(Allocation {
                id: u.id.clone(),
                size: u.size,
                tier,
            });
            assigned.push(Unit { tier, ..u });
        }
        s.units = s
            .units
            .iter()
            .map(|u| {
                assigned
                    .iter()
                    .find(|a| a.id == u.id)
                    .cloned()
                    .unwrap_or_else(|| u.clone())
            })
            .collect();
        s.layer = LayerState::default();
        s.phase = Phase::Working { layer: i };
        for u in &assigned {
            let vars = work_vars(s, u);
            call(
                s,
                CallSpec {
                    label: format!("work:{}", u.id),
                    role: Role::Worker,
                    unit: Some(&u.id),
                    tier: u.tier,
                    effort: None,
                    template: &pipeline.worker_prompt,
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
    review::start(s, out);
}

pub fn on_work(
    s: &mut RunState,
    layer: usize,
    answers: &[(String, Answer)],
    out: &mut Vec<Command>,
) {
    let mut ok = Vec::new();
    for id in s.layers[layer].clone() {
        let label = format!("work:{id}");
        let Some(a) = answer_for(answers, &label) else {
            continue;
        };
        let u = s.unit(&id).expect("known unit").clone();
        let why = match a {
            Answer::Reply(Reply::Work(w), counts) => match &w.blocked {
                Some(b) => Some(format!("blocked: {b}")),
                None => {
                    ok.push(Unit {
                        counts_for_tier: *counts,
                        ..u.clone()
                    });
                    None
                }
            },
            _ => Some(String::from("worker died")),
        };
        if let Some(why) = why {
            // A planning problem, not model evidence.
            add(&mut s.blocked, &u.id);
            s.stuck.push(UnitNote {
                id: u.id.clone(),
                goal: u.goal.clone(),
                why,
            });
        }
    }
    if ok.is_empty() {
        start_layer(s, layer + 1, out);
        return;
    }
    s.layer = LayerState {
        ok,
        ..LayerState::default()
    };
    s.phase = Phase::Checking { layer, round: 0 };
    let gate = s.gate.clone();
    checks(s, format!("check:L{}", layer + 1), gate, out);
}

fn on_layer_check(
    s: &mut RunState,
    layer: usize,
    round: u32,
    results: &[CheckOutcome],
    changed: &[String],
    out: &mut Vec<Command>,
) {
    let failed: Vec<CheckOutcome> = results
        .iter()
        .filter(|r| r.exit_code != 0)
        .cloned()
        .collect();
    let max = s.pipeline().max_fix_rounds;
    let l = format!("L{}", layer + 1);
    if !failed.is_empty() && round < max {
        let round = round + 1;
        let culprits = blame(&failed, &s.layer.ok);
        let who: Vec<String> = culprits
            .iter()
            .map(|u| format!("{} on {}", u.id, fix_tier(u.tier, round).name()))
            .collect();
        let cmds: Vec<&str> = failed.iter().map(|f| f.cmd.as_str()).collect();
        out.push(Command::Log(format!(
            "{l}: {} failed; fix {round}/{max} for {}",
            cmds.join(" | "),
            who.join(", ")
        )));
        let problem = check_problem(&failed);
        s.phase = Phase::Fixing { layer, round };
        s.layer.culprits = culprits.clone();
        s.layer.failed = failed;
        fix_calls(
            s,
            &culprits,
            &problem,
            (round, max),
            true,
            &|u| format!("fix:{u}#{round}"),
            out,
        );
        return;
    }
    s.changed = changed.to_vec();
    for u in s.layer.ok.clone() {
        if u.counts_for_tier {
            s.outcomes.push(ModelOutcome {
                key: stat_key(u.size),
                tier: u.tier,
                pass: !s.layer.needs_fix.contains(&u.id),
            });
        }
    }
    if failed.is_empty() {
        for sig in s.layer.sigs.clone() {
            add(&mut s.fixed_sigs, &sig);
        }
        for u in s.layer.ok.clone() {
            s.done.push(u.id.clone());
            s.built.push(u);
        }
    } else {
        let cmds: Vec<&str> = failed.iter().map(|f| f.cmd.as_str()).collect();
        let why = format!(
            "checks still failing after {max} fix rounds: {}",
            cmds.join(" | ")
        );
        for u in s.layer.ok.clone() {
            add(&mut s.blocked, &u.id);
            s.stuck.push(UnitNote {
                id: u.id.clone(),
                goal: u.goal.clone(),
                why: why.clone(),
            });
        }
        s.stopped = true;
    }
    start_layer(s, layer + 1, out);
}

pub fn on_fix(
    s: &mut RunState,
    layer: usize,
    round: u32,
    answers: &[(String, Answer)],
    out: &mut Vec<Command>,
) {
    record_fixes(s, format!("L{}", layer + 1), round, answers, &|u| {
        format!("fix:{u}#{round}")
    });
    s.phase = Phase::Checking { layer, round };
    let gate = s.gate.clone();
    checks(s, format!("check:L{}", layer + 1), gate, out);
}

/// The build fix loop's calls (design §6.4 step 5), shared by every fix round of the chain: one
/// Fixer per culprit at its fix tier, with the dev pipeline's fix prompt and limits. `(round, max)`
/// is the round and the stage's rounds; `high_last` gives the last round high effort.
pub fn fix_calls(
    s: &mut RunState,
    culprits: &[Unit],
    problem: &str,
    (round, max): (u32, u32),
    high_last: bool,
    label: &dyn Fn(&str) -> String,
    out: &mut Vec<Command>,
) {
    let pipeline = s.pipeline().clone();
    for u in culprits {
        let vars = fix_vars(s, u, problem, round, max);
        call(
            s,
            CallSpec {
                label: label(&u.id),
                role: Role::Fixer,
                unit: Some(&u.id),
                tier: fix_tier(u.tier, round),
                effort: (high_last && round == max).then_some(Effort::High),
                template: &pipeline.fix_prompt,
                vars,
                schema: SchemaId::Work,
                max_turns: pipeline.build_max_turns,
                timeout_secs: pipeline.build_timeout_secs,
            },
            out,
        );
    }
}

/// Records a fix round over failed checks: a fixer that proves innocence clears its unit, and the
/// failures' signatures are kept for learn and for the fix record.
pub fn record_fixes(
    s: &mut RunState,
    layer: String,
    round: u32,
    answers: &[(String, Answer)],
    label: &dyn Fn(&str) -> String,
) {
    let culprits = s.layer.culprits.clone();
    let mut summaries = Vec::new();
    for u in &culprits {
        let a = answer_for(answers, &label(&u.id));
        let innocent = matches!(a, Some(Answer::Reply(Reply::Work(w), _)) if w.blocked.is_some());
        if !innocent {
            add(&mut s.layer.needs_fix, &u.id);
        }
        summaries.push(summary(a));
    }
    let failed: Vec<FailedCheck> = s
        .layer
        .failed
        .iter()
        .map(|f| FailedCheck {
            cmd: f.cmd.clone(),
            exit_code: f.exit_code,
            sig: signature(&f.cmd, &f.output_tail),
            output_tail: last_chars(&f.output_tail, 1000),
        })
        .collect();
    for f in &failed {
        s.layer.sigs.push(f.sig.clone());
    }
    s.fixes.push(FixRecord {
        layer,
        round,
        units: culprits.iter().map(|u| u.id.clone()).collect(),
        failed,
        fix_summaries: summaries,
    });
}

/// What a fixer said, for the learn evidence and the review-fix record.
pub fn summary(a: Option<&Answer>) -> String {
    match a {
        Some(Answer::Reply(Reply::Work(w), _)) => match &w.blocked {
            Some(b) => format!("blocked: {b}"),
            None => w.summary.clone(),
        },
        _ => String::from("fixer died"),
    }
}
