//! Publish (design §6.1): the `publish` stage asks the kernel for a draft PR; the kernel's answer
//! moves the chain on, or leaves the run stuck. The kernel checks every invariant; this only asks.

use crate::api::Command;
use crate::model::{Phase, RunState};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Longest description the kernel accepts in a PR title (design §4.2).
const MAX_DESCRIPTION: usize = 60;

/// A conventional-commit title from the task's one-line name: control characters and runs of
/// whitespace become one space, and the description is cut to 60 characters.
pub fn title(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    let cut: String = joined.chars().take(MAX_DESCRIPTION).collect();
    let cut = cut.trim_end();
    format!(
        "feat: {}",
        if cut.is_empty() {
            "workgraph task"
        } else {
            cut
        }
    )
}

fn held(s: &mut RunState, out: &mut Vec<Command>, what: &str) -> bool {
    if s.phase != Phase::Publishing {
        out.push(Command::Log(format!(
            "ignored {what}: no publish was requested"
        )));
        return false;
    }
    true
}

pub fn on_published(s: &mut RunState, url: &str, out: &mut Vec<Command>) {
    if held(s, out, "a published PR") {
        out.push(Command::Log(format!("draft PR: {url}")));
        let at = s.stage_at.map_or(s.stages.len(), |i| i + 1);
        super::stages::next(s, at, out);
    }
}

/// A refused publish leaves the run stuck, with the kernel's reason as its note.
pub fn on_refused(s: &mut RunState, why: &str, out: &mut Vec<Command>) {
    if held(s, out, "a publish refusal") {
        s.phase = Phase::Done;
        let o = super::stages::settled(s, Some(format!("publish refused: {why}")), Vec::new());
        out.push(Command::Finish(o));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Outcome, PublishDef, RulesReport, StageDef, StageKind, Status};
    use alloc::vec;

    fn state() -> RunState {
        RunState {
            title: String::from("Add things"),
            stages: vec![StageDef {
                id: String::from("publish"),
                label: String::from("publish"),
                kind: StageKind::Publish(PublishDef {
                    draft: true,
                    labels: vec![],
                }),
            }],
            ..RunState::default()
        }
    }

    fn outcome(status: Status) -> Outcome {
        Outcome {
            status,
            note: None,
            review: None,
            done: vec![],
            skipped: vec![],
            stuck: vec![],
            fix_rounds: 0,
            review_fix_rounds: 0,
            out_of_scope: vec![],
            committable: vec![],
            models: vec![],
            rules: RulesReport::default(),
            proposals: vec![],
        }
    }

    #[test]
    fn a_ready_run_publishes_then_finishes_with_the_kernels_answer() {
        let mut s = state();
        let mut out = Vec::new();
        super::super::stages::begin(&mut s, outcome(Status::ReadyForReview), &mut out);
        assert_eq!(
            out,
            vec![Command::Publish {
                title: String::from("feat: Add things")
            }]
        );
        let mut done = Vec::new();
        on_published(&mut s, "https://x/pull/1", &mut done);
        assert!(
            matches!(done.last(), Some(Command::Finish(o)) if o.status == Status::ReadyForReview)
        );
        assert_eq!(s.phase, Phase::Done);

        let mut s = state();
        super::super::stages::begin(&mut s, outcome(Status::ReadyForReview), &mut Vec::new());
        let mut refused = Vec::new();
        on_refused(&mut s, "no review", &mut refused);
        assert!(
            matches!(&refused[0], Command::Finish(o) if o.status == Status::Stuck
            && o.note.as_deref() == Some("publish refused: no review"))
        );
        let mut late = Vec::new();
        on_published(&mut s, "u", &mut late);
        assert!(
            matches!(&late[0], Command::Log(_)),
            "an answer nobody asked for is only logged"
        );
    }

    #[test]
    fn stuck_runs_and_runs_without_later_stages_finish_at_once() {
        for (stages, status) in [(true, Status::Stuck), (false, Status::ReadyForReview)] {
            let mut s = state();
            if !stages {
                s.stages.clear();
            }
            let mut out = Vec::new();
            super::super::stages::begin(&mut s, outcome(status), &mut out);
            assert!(matches!(&out[..], [Command::Finish(_)]));
        }
    }

    #[test]
    fn titles_are_one_conventional_line_of_at_most_60_characters() {
        assert_eq!(title("Fix add so it adds"), "feat: Fix add so it adds");
        assert_eq!(title("  a\n\tb\u{7}c  "), "feat: a b c");
        assert_eq!(title(""), "feat: workgraph task");
        let long = title(&"x".repeat(200));
        assert_eq!(long.len(), "feat: ".len() + 60);
    }
}
