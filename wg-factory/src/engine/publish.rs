//! Publish (design §6.1, Phase 2): a run that is ready for review asks the kernel for a draft PR,
//! then finishes with the kernel's answer. The kernel checks every invariant; this only asks.

use crate::api::{Command, Outcome, Status};
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

/// Finishes now, or first asks the kernel to publish a run that is ready for review.
pub fn finish_or_publish(s: &mut RunState, o: Outcome, out: &mut Vec<Command>) {
    let publish = s.pipeline.as_ref().is_some_and(|p| p.publish.is_some());
    if publish && o.status == Status::ReadyForReview {
        s.phase = Phase::Publishing;
        s.finished = Some(o);
        out.push(Command::Publish {
            title: title(&s.title),
        });
        return;
    }
    s.phase = Phase::Done;
    out.push(Command::Finish(o));
}

fn held(s: &mut RunState, out: &mut Vec<Command>, what: &str) -> Option<Outcome> {
    if s.phase != Phase::Publishing {
        out.push(Command::Log(format!(
            "ignored {what}: no publish was requested"
        )));
        return None;
    }
    s.phase = Phase::Done;
    s.finished.take()
}

pub fn on_published(s: &mut RunState, url: &str, out: &mut Vec<Command>) {
    if let Some(o) = held(s, out, "a published PR") {
        out.push(Command::Log(format!("draft PR: {url}")));
        out.push(Command::Finish(o));
    }
}

/// A refused publish leaves the run stuck, with the kernel's reason as its note.
pub fn on_refused(s: &mut RunState, why: &str, out: &mut Vec<Command>) {
    if let Some(o) = held(s, out, "a publish refusal") {
        out.push(Command::Finish(Outcome {
            status: Status::Stuck,
            note: Some(format!("publish refused: {why}")),
            ..o
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{LlmStage, PipelineDef, PublishDef, RulesReport, Tier};
    use alloc::vec;

    fn stage() -> LlmStage {
        LlmStage {
            tier: Tier::Opus,
            effort: None,
            prompt: String::from("p"),
            max_turns: 1,
            timeout_secs: 1,
        }
    }

    fn state(publish: bool) -> RunState {
        RunState {
            title: String::from("Add things"),
            pipeline: Some(PipelineDef {
                read: stage(),
                challenge: stage(),
                max_units: 5,
                max_fix_rounds: 3,
                build_max_turns: 80,
                build_timeout_secs: 1800,
                worker_prompt: String::from("work"),
                fix_prompt: String::from("fix"),
                review: stage(),
                review_max_fix_rounds: 1,
                learn: stage(),
                publish: publish.then(|| PublishDef {
                    draft: true,
                    labels: vec![],
                }),
            }),
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
        let mut s = state(true);
        let mut out = Vec::new();
        finish_or_publish(&mut s, outcome(Status::ReadyForReview), &mut out);
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

        let mut s = state(true);
        finish_or_publish(&mut s, outcome(Status::ReadyForReview), &mut Vec::new());
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
    fn stuck_runs_and_runs_without_a_publish_stage_finish_at_once() {
        for (publish, status) in [(true, Status::Stuck), (false, Status::ReadyForReview)] {
            let mut s = state(publish);
            let mut out = Vec::new();
            finish_or_publish(&mut s, outcome(status), &mut out);
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
