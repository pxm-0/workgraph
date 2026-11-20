//! The stage machine after dev, alone (design §6.1, Phase 4): which command each kind emits, when a
//! stage is skipped, and how its answer moves the chain on or stops it.

use wg_factory::api::*;
use wg_factory::engine::stages::{begin, on_gate, on_sandbox};
use wg_factory::model::{Phase, RunState};

fn outcome() -> Outcome {
    Outcome {
        status: Status::ReadyForReview,
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

fn st(id: &str, kind: StageKind) -> StageDef {
    StageDef {
        id: id.into(),
        label: id.replace('-', ""),
        kind,
    }
}

fn release_line(tainted: bool) -> RunState {
    RunState {
        risk: RiskView {
            tainted,
            ..RiskView::default()
        },
        stages: vec![
            st(
                "rehearse",
                StageKind::Sandbox {
                    steps_from: "release_rehearsal".into(),
                    always: false,
                },
            ),
            st(
                "gate",
                StageKind::HaruspexGate {
                    gate: "ready-to-release".into(),
                    expect_only: vec!["pending".into()],
                },
            ),
            st(
                "owner",
                StageKind::Human {
                    gate: "ready-to-release".into(),
                    ask: "decide".into(),
                },
            ),
        ],
        ..RunState::default()
    }
}

fn outcome_of(cmds: &[Command]) -> &Outcome {
    match cmds.last() {
        Some(Command::Finish(o)) => o,
        other => panic!("no finish: {other:?}"),
    }
}

#[test]
fn a_risky_only_sandbox_stage_is_skipped_for_an_untainted_task_then_the_gate_then_the_owner() {
    let mut s = release_line(false);
    let mut out = vec![];
    begin(&mut s, outcome(), &mut out);
    assert_eq!(
        out,
        [Command::Gate {
            label: "gate".into(),
            gate: "ready-to-release".into()
        }]
    );
    let mut out = vec![];
    on_gate(&mut s, "gate", &["pending".into()], &mut out);
    assert_eq!(
        out,
        [Command::Human {
            gate: "ready-to-release".into(),
            ask: "decide".into()
        }]
    );
    assert_eq!(s.phase, Phase::Done);
}

#[test]
fn a_tainted_task_rehearses_and_a_failed_step_or_blocker_stops_the_chain() {
    let mut s = release_line(true);
    let mut out = vec![];
    begin(&mut s, outcome(), &mut out);
    assert!(matches!(&out[..], [Command::Sandbox { label, .. }] if label == "rehearse"));
    let mut late = vec![];
    on_gate(&mut s, "gate", &[], &mut late);
    assert!(matches!(&late[..], [Command::Log(_)]), "not asked for yet");
    let failed = CheckOutcome {
        name: "release-rehearsal-1".into(),
        cmd: "migrate".into(),
        exit_code: 2,
        output_tail: "boom".into(),
    };
    let mut out = vec![];
    on_sandbox(&mut s, "rehearse", &[failed], &mut out);
    let o = outcome_of(&out);
    assert_eq!(o.status, Status::Stuck);
    assert_eq!(
        o.note.as_deref(),
        Some("stage rehearse: sandbox steps failed: release-rehearsal-1 (exit 2)")
    );

    let mut s = release_line(true);
    begin(&mut s, outcome(), &mut vec![]);
    on_sandbox(&mut s, "rehearse", &[], &mut vec![]);
    let mut out = vec![];
    on_gate(
        &mut s,
        "gate",
        &["pending".into(), "plan missing".into()],
        &mut out,
    );
    assert!(outcome_of(&out)
        .note
        .as_deref()
        .unwrap()
        .contains("other blockers: plan missing"));
}

#[test]
fn a_run_that_is_not_ready_skips_every_later_stage() {
    let mut s = release_line(true);
    let mut out = vec![];
    begin(
        &mut s,
        Outcome {
            status: Status::Stuck,
            ..outcome()
        },
        &mut out,
    );
    assert_eq!(outcome_of(&out).status, Status::Stuck);
    assert_eq!(out.len(), 1);
}

#[test]
fn reply_flags_name_the_gate_fields_of_each_schema() {
    let v = Reply::Verdict(Verdict {
        passed: false,
        findings: vec![],
    });
    assert_eq!(v.flag("passed"), Some(false));
    assert_eq!(v.flag("ready"), None);
    let p = Reply::ReleasePlan(ReleasePlan {
        ready: true,
        summary: "s".into(),
    });
    assert_eq!(p.flag("ready"), Some(true));
    assert!(p.findings().is_empty());
}
