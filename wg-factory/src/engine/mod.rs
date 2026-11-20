//! The dev engine: `step(state, event) -> (state, commands)`, a port of run.js's control flow
//! (design §6.4). Each phase waits until every outstanding label has answered.

pub mod build;
pub mod learn;
pub mod read;
pub mod review;
pub mod vars;

use crate::api::{Command, Effort, Event, LlmCall, Role, SchemaId, Tier};
use crate::model::{Answer, Phase, RunState};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Advances the run by one event. Never mutates `state`; returns the next state and commands.
pub fn step(state: &RunState, ev: &Event) -> (RunState, Vec<Command>) {
    let mut s = state.clone();
    let mut out = Vec::new();
    match ev {
        Event::Started(input) => read::start(&mut s, input, &mut out),
        Event::RiskUpdated(r) => s.risk = r.clone(),
        Event::KernelEscalations(e) => s.kernel_escalations = e.clone(),
        Event::LlmDone {
            label,
            reply,
            counts_for_tier,
            ..
        } => answer(
            &mut s,
            label,
            Answer::Reply(reply.clone(), *counts_for_tier),
            &mut out,
        ),
        Event::LlmFailed { label, why } => answer(&mut s, label, Answer::Failed(*why), &mut out),
        Event::ChecksDone {
            label,
            results,
            changed_files,
        } => {
            if s.pending.iter().any(|p| p == label) {
                s.pending.clear();
                build::on_checks(&mut s, results, changed_files, &mut out);
            } else {
                out.push(Command::Log(alloc::format!(
                    "ignored check result for {label}, which was not requested"
                )));
            }
        }
        Event::ChecksUnavailable { label, why } => {
            s.phase = Phase::Done;
            out.push(Command::Log(alloc::format!("{label}: {why}")));
            out.push(Command::Finish(learn::failed(
                &s,
                alloc::format!(
                    "{label}: no verified check result, so nothing can be trusted. Edits so far are uncommitted in the working tree."
                ),
            )));
        }
    }
    (s, out)
}

/// Collects one call's answer; dispatches when the whole batch has answered.
fn answer(s: &mut RunState, label: &str, a: Answer, out: &mut Vec<Command>) {
    let Some(i) = s.pending.iter().position(|p| p == label) else {
        out.push(Command::Log(alloc::format!(
            "ignored an answer for {label}, which was not requested"
        )));
        return;
    };
    s.pending.remove(i);
    s.answers.push((label.to_string(), a));
    if !s.pending.is_empty() {
        return;
    }
    let answers = core::mem::take(&mut s.answers);
    match s.phase.clone() {
        Phase::Reading => read::on_read(s, &answers[0].1, out),
        Phase::Challenging => read::on_challenge(s, &answers[0].1, out),
        Phase::Working { layer } => build::on_work(s, layer, &answers, out),
        Phase::Fixing { layer, round } => build::on_fix(s, layer, round, &answers, out),
        Phase::Reviewing { round } => review::on_review(s, round, &answers[0].1, out),
        Phase::ReviewFixing { round } => review::on_fix(s, round, &answers, out),
        Phase::Learning => learn::on_learn(s, &answers[0].1, out),
        other => out.push(Command::Log(alloc::format!(
            "ignored LLM answers in phase {other:?}"
        ))),
    }
}

/// The answer for `label` in a batch.
pub fn answer_for<'a>(answers: &'a [(String, Answer)], label: &str) -> Option<&'a Answer> {
    answers.iter().find(|(l, _)| l == label).map(|(_, a)| a)
}

pub struct CallSpec<'a> {
    pub label: String,
    pub role: Role,
    pub unit: Option<&'a str>,
    pub tier: Tier,
    pub effort: Option<Effort>,
    pub template: &'a str,
    pub vars: vars::Vars,
    pub schema: SchemaId,
    pub max_turns: u32,
    pub timeout_secs: u32,
}

/// Emits an LLM call and marks its label outstanding.
pub fn call(s: &mut RunState, c: CallSpec, out: &mut Vec<Command>) {
    s.pending.push(c.label.clone());
    out.push(Command::Llm(LlmCall {
        label: c.label,
        role: c.role,
        unit: c.unit.map(ToString::to_string),
        tier: c.tier,
        effort: c.effort,
        template: c.template.to_string(),
        vars: c.vars,
        schema: c.schema,
        max_turns: c.max_turns,
        timeout_secs: c.timeout_secs,
    }));
}

/// Emits a check run over the gate (or every check, for the baseline) and marks it outstanding.
pub fn checks(s: &mut RunState, label: String, names: Vec<String>, out: &mut Vec<Command>) {
    s.pending.push(label.clone());
    out.push(Command::Checks { label, names });
}
