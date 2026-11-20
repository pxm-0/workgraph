//! Read, challenge, criteria merge and escalations (design §6.4 steps 1-4).

use super::vars::{challenge_vars, read_vars};
use super::{call, checks, CallSpec};
use crate::api::{
    Challenge, Command, Escalation, LlmFailure, Reply, Role, RunInput, SchemaId, Severity,
};
use crate::engine::learn;
use crate::model::{Answer, Phase, RunState, Unit};
use crate::plan::scope::{committable, dirty_overlap};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub fn start(s: &mut RunState, input: &RunInput, out: &mut Vec<Command>) {
    s.task = input.task.clone();
    s.title = input.title.clone();
    s.decisions = input.decisions.clone();
    s.resume_task = input.resume_task.clone();
    s.initialized_earlier = input.initialized_earlier;
    s.user_changes_before = input.user_changes_before.clone();
    s.memory = input.memory.clone();
    s.can_save = input.can_save;
    s.pipeline = Some(input.pipeline.clone());
    if !input.can_save {
        out.push(Command::Log(String::from(
            "memory cannot be saved this run (no recall, or the file is not valid); running without learning",
        )));
    }
    let stage = s.pipeline().read.clone();
    let vars = read_vars(s);
    s.phase = Phase::Reading;
    call(
        s,
        CallSpec {
            label: String::from("read"),
            role: Role::Reader,
            unit: None,
            tier: stage.tier,
            effort: stage.effort,
            template: &stage.prompt,
            vars,
            schema: SchemaId::Reading,
            max_turns: stage.max_turns,
            timeout_secs: stage.timeout_secs,
        },
        out,
    );
}

pub fn on_read(s: &mut RunState, a: &Answer, out: &mut Vec<Command>) {
    let reading = match a {
        Answer::Reply(Reply::Reading(r), _) => r.clone(),
        _ => {
            s.phase = Phase::Done;
            out.push(Command::Finish(learn::failed(
                s,
                String::from("the Reading agent died; no source code was changed"),
            )));
            return;
        }
    };
    if reading.units.is_empty() {
        let note = reading
            .note
            .clone()
            .unwrap_or_else(|| String::from("the Reading found nothing to change"));
        s.reading = Some(reading);
        s.phase = Phase::Done;
        let mut o = learn::failed(s, note);
        o.status = crate::api::Status::NothingToDo;
        out.push(Command::Finish(o));
        return;
    }
    s.units = reading
        .units
        .iter()
        .map(|u| Unit {
            id: u.id.clone(),
            goal: u.goal.clone(),
            files: u.files.clone(),
            depends_on: u.depends_on.clone(),
            criteria: u.criteria.clone(),
            size: u.size,
            risky: u.risky,
            risk_reason: u.risk_reason.clone(),
            tier: crate::api::Tier::Sonnet,
            counts_for_tier: true,
        })
        .collect();
    s.check_cmds = reading
        .checks
        .iter()
        .map(|c| (c.name.clone(), c.command.clone()))
        .collect();
    let task_id = reading.task_id.clone();
    s.reading = Some(reading);
    let stage = s.pipeline().challenge.clone();
    let vars = challenge_vars(s, &task_id);
    s.phase = Phase::Challenging;
    call(
        s,
        CallSpec {
            label: String::from("challenge"),
            role: Role::Challenger,
            unit: None,
            tier: stage.tier,
            effort: stage.effort,
            template: &stage.prompt,
            vars,
            schema: SchemaId::Challenge,
            max_turns: stage.max_turns,
            timeout_secs: stage.timeout_secs,
        },
        out,
    );
}

pub fn on_challenge(s: &mut RunState, a: &Answer, out: &mut Vec<Command>) {
    let challenge: Option<Challenge> = match a {
        Answer::Reply(Reply::Challenge(c), _) => Some(c.clone()),
        Answer::Failed(why) => {
            let what = if *why == LlmFailure::Invalid {
                "returned invalid output twice"
            } else {
                "died"
            };
            out.push(Command::Log(format!(
                "the challenge agent {what}; continuing with the Reading as written"
            )));
            None
        }
        Answer::Reply(..) => None,
    };
    let assign = challenge
        .as_ref()
        .map(|c| c.assign.clone())
        .unwrap_or_default();
    s.units = s
        .units
        .iter()
        .map(|u| {
            let mut criteria = u.criteria.clone();
            for a in assign.iter().filter(|a| a.unit_id == u.id) {
                if !criteria.contains(&a.criterion_id) {
                    criteria.push(a.criterion_id.clone());
                }
            }
            Unit {
                criteria,
                ..u.clone()
            }
        })
        .collect();
    let unplaced: Vec<String> = assign
        .iter()
        .filter(|a| !s.units.iter().any(|u| u.id == a.unit_id))
        .map(|a| a.criterion_id.clone())
        .collect();
    if !unplaced.is_empty() {
        out.push(Command::Log(format!(
            "challenge criteria assigned to unknown units, left to the reviewer: {}",
            unplaced.join(", ")
        )));
    }
    s.challenge = challenge;
    let overlap = dirty_overlap(&s.units, &s.user_changes_before);
    let dirty_decided = s.decisions.iter().any(|d| d.id == "DIRTY");
    let mut escalations = s
        .reading
        .as_ref()
        .map(|r| r.escalations.clone())
        .unwrap_or_default();
    if let Some(c) = &s.challenge {
        escalations.extend(c.escalations.iter().cloned());
    }
    if !overlap.is_empty() && !dirty_decided {
        escalations.push(dirty_escalation(&overlap));
    }
    for e in &s.kernel_escalations {
        if !escalations.iter().any(|x| x.id == e.id) {
            escalations.push(e.clone());
        }
    }
    if !escalations.is_empty() {
        s.phase = Phase::Done;
        out.push(Command::Escalate(escalations));
        return;
    }
    s.committable = committable(&s.user_changes_before, &overlap, dirty_decided);
    let names: Vec<String> = s.check_cmds.iter().map(|(n, _)| n.clone()).collect();
    s.phase = Phase::Baseline;
    checks(s, String::from("check:baseline"), names, out);
}

/// A planned edit on top of the user's uncommitted work would mix their changes into the commit.
pub fn dirty_escalation(overlap: &[String]) -> Escalation {
    Escalation {
        id: String::from("DIRTY"),
        severity: Severity::High,
        question: format!(
            "The plan edits files that hold your uncommitted work ({}). Commit or stash them first, or let the run edit on top so your changes go into its commit?",
            overlap.join(", ")
        ),
        recommendation: String::from("Commit or stash them first, then rerun"),
        options: alloc::vec![
            String::from("Commit or stash them first"),
            String::from("Edit on top; commit my changes with the run"),
        ],
    }
}
