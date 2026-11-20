//! Hostile-factory and hostile-agent flow tests (design §15.2): a factory or an agent that tries
//! to get past the kernel, and the kernel refusing each attempt.

mod support;

use serde_json::{json, Value};
use std::sync::Arc;
use support::*;
use wg_factory::api::{Command, Event, LlmCall, Tier};
use wg_factory::model::RunState;

fn real(state: &RunState, ev: &Event) -> (RunState, Vec<Command>) {
    wg_factory::engine::step(state, ev)
}

fn refusals(o: &Out) -> Vec<String> {
    o.journal
        .iter()
        .filter_map(|e| e["refused"].as_str().map(str::to_string))
        .collect()
}

fn proc_labels(o: &Out) -> Vec<String> {
    o.proc_calls.iter().map(|c| c.label.clone()).collect()
}

#[test]
fn bad_labels_and_templates_are_refused_and_never_become_paths() {
    let long = "a".repeat(200);
    let bad: Vec<(String, String)> = vec![
        (
            "../../../../Library/LaunchAgents/x.plist".into(),
            "read".into(),
        ),
        ("a/b".into(), "read".into()),
        (long, "read".into()),
        ("read!retry".into(), "../etc/passwd".into()),
    ];
    let step = move |s: &RunState, ev: &Event| {
        let (next, mut cmds) = real(s, ev);
        if matches!(ev, Event::Started(_)) {
            let Command::Llm(read) = cmds[0].clone() else {
                unreachable!()
            };
            for (label, template) in &bad {
                cmds.push(Command::Llm(LlmCall {
                    label: label.clone(),
                    template: template.clone(),
                    ..read.clone()
                }));
            }
        }
        (next, cmds)
    };
    let o = run_with(
        Opts {
            units: vec![unit("a", json!({}))],
            ..Opts::default()
        },
        &step,
    );
    assert_eq!(
        o.result["status"], "ready_for_review",
        "the honest calls still ran"
    );
    let r = refusals(&o);
    assert_eq!(
        r.iter()
            .filter(|w| w.contains("not a valid command label"))
            .count(),
        3,
        "{r:?}"
    );
    assert!(r.iter().any(|w| w.contains("unknown template")));
    assert!(proc_labels(&o)
        .iter()
        .all(|l| !l.contains('/') && l.len() < 40));
    assert!(!exists_under(&o.home, "LaunchAgents") && !exists_under(&o.home, "x.plist"));
    for p in walk(&o.run_dir.join("logs")) {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c)),
            "{name}"
        );
    }
}

#[test]
fn a_factory_that_drops_every_check_still_gets_the_kernels_gate() {
    let step = |s: &RunState, ev: &Event| {
        let (next, cmds) = real(s, ev);
        let cmds = cmds
            .into_iter()
            .map(|c| match c {
                Command::Checks { label, .. } if label != "check:baseline" => Command::Checks {
                    label,
                    names: vec![],
                },
                c => c,
            })
            .collect();
        (next, cmds)
    };
    let o = run_with(
        Opts {
            units: vec![unit("a", json!({}))],
            checks: vec![("test", "npm test"), ("lint", "npm run lint")],
            ..Opts::default()
        },
        &step,
    );
    let l1 = o
        .journal
        .iter()
        .find(|e| e["kind"] == "checks_run" && e["label"] == "check:L1")
        .unwrap();
    assert_eq!(
        l1["results"],
        json!([{ "name": "test", "exit": 0 }, { "name": "lint", "exit": 0 }])
    );
    assert_eq!(o.result["checks"], json!(["test", "lint"]));
}

#[test]
fn a_factory_that_claims_ready_without_building_or_review_is_not_reviewed() {
    let step = |s: &RunState, ev: &Event| {
        let (next, cmds) = real(s, ev);
        if let Event::ChecksDone { label, .. } = ev {
            if label == "check:baseline" {
                let mut o = wg_factory::engine::learn::failed(&next, "done".into());
                o.status = wg_factory::api::Status::ReadyForReview;
                return (next, vec![Command::Finish(o)]);
            }
        }
        (next, cmds)
    };
    let o = run_with(
        Opts {
            units: vec![unit("a", json!({}))],
            ..Opts::default()
        },
        &step,
    );
    assert_eq!(o.result["status"], "ready_for_review");
    assert_eq!(
        o.result["reviewed"], false,
        "only the kernel's ReviewPass makes a run reviewed"
    );
    let tokens = std::fs::read_to_string(o.run_dir.join("tokens.jsonl")).unwrap();
    assert!(
        !tokens.contains("\\\"kind\\\":\\\"review\\\""),
        "no review token to finalize with"
    );
}

#[test]
fn dropping_a_critical_escalation_and_emitting_workers_ends_needs_decision() {
    let step = |s: &RunState, ev: &Event| {
        let (next, cmds) = real(s, ev);
        let dropped = cmds.into_iter().map(|c| match c {
            Command::Escalate(_) => {
                let read = next.units[0].clone();
                Command::Llm(LlmCall {
                    label: format!("work:{}", read.id),
                    role: wg_factory::api::Role::Worker,
                    unit: Some(read.id.clone()),
                    tier: Tier::Haiku,
                    effort: None,
                    template: "work".into(),
                    vars: vec![],
                    schema: wg_factory::api::SchemaId::Work,
                    max_turns: 10,
                    timeout_secs: 60,
                })
            }
            c => c,
        });
        let dropped: Vec<Command> = dropped.collect();
        (next, dropped)
    };
    let esc =
        json!({ "id": "FM-002", "severity": "critical", "question": "q", "recommendation": "r" });
    let o = run_with(
        Opts {
            units: vec![unit("a", json!({}))],
            escalations: vec![esc],
            ..Opts::default()
        },
        &step,
    );
    assert_eq!(o.result["status"], "needs_decision");
    assert_eq!(ids(&o.result["escalations"]), ["FM-002"]);
    assert!(!proc_labels(&o).contains(&"work:a".to_string()));
}

#[test]
fn skipping_the_baseline_and_emitting_workers_is_refused() {
    let step = |s: &RunState, ev: &Event| {
        let (next, cmds) = real(s, ev);
        let swapped = cmds.into_iter().map(|c| match c {
            Command::Checks { label, .. } if label == "check:baseline" => Command::Llm(LlmCall {
                label: "work:a".into(),
                role: wg_factory::api::Role::Worker,
                unit: Some("a".into()),
                tier: Tier::Haiku,
                effort: None,
                template: "work".into(),
                vars: vec![],
                schema: wg_factory::api::SchemaId::Work,
                max_turns: 10,
                timeout_secs: 60,
            }),
            c => c,
        });
        (next, swapped.collect())
    };
    let o = run_with(
        Opts {
            units: vec![unit("a", json!({}))],
            ..Opts::default()
        },
        &step,
    );
    assert!(refusals(&o)
        .iter()
        .any(|r| r.contains("no edit may precede")));
    assert!(!proc_labels(&o).contains(&"work:a".to_string()));
    assert_eq!(o.result["status"], "failed");
}

#[test]
fn a_reviewer_on_sonnet_is_refused_and_mints_no_review_pass() {
    let step = |s: &RunState, ev: &Event| {
        let (next, cmds) = real(s, ev);
        let cmds = cmds
            .into_iter()
            .map(|c| match c {
                Command::Llm(call) if call.label.starts_with("review#") => Command::Llm(LlmCall {
                    tier: Tier::Sonnet,
                    ..call
                }),
                c => c,
            })
            .collect();
        (next, cmds)
    };
    let o = run_with(
        Opts {
            units: vec![unit("a", json!({}))],
            ..Opts::default()
        },
        &step,
    );
    assert!(refusals(&o).iter().any(|r| r.contains("must be opus")));
    assert!(!proc_labels(&o).iter().any(|l| l.starts_with("review")));
    assert_eq!(o.result["reviewed"], false);
}

#[test]
fn a_unit_the_kernel_holds_risky_is_refused_even_when_the_factory_asks() {
    let m = || {
        unit(
            "m",
            json!({ "files": ["db/migrations/001_add.sql"], "criteria": ["AC-m"] }),
        )
    };
    let honest = run(Opts {
        units: vec![m()],
        ..Opts::default()
    });
    assert!(honest.result["skipped"][0]["why"]
        .as_str()
        .unwrap()
        .contains("a database migration"));
    let blind = |s: &RunState, ev: &Event| match ev {
        Event::RiskUpdated(_) => (s.clone(), vec![]),
        _ => real(s, ev),
    };
    let o = run_with(
        Opts {
            units: vec![m()],
            ..Opts::default()
        },
        &blind,
    );
    assert!(refusals(&o).iter().any(|r| r.contains("is risky")));
    assert!(!proc_labels(&o).contains(&"work:m".to_string()));
    assert_eq!(o.result["stuck"][0]["why"], "worker died");
}

fn agent_edits_task(edit: fn(&mut Value)) -> Out {
    let hx = Arc::new(fake_hx(&[unit("a", json!({}))], &[("test", "npm test")]));
    let h = hx.clone();
    run(Opts {
        units: vec![unit("a", json!({}))],
        hx: Some(hx),
        answer: Arc::new(move |label, _| {
            if label == "work:a" {
                edit(&mut h.task.lock().unwrap());
            }
            None
        }),
        ..Opts::default()
    })
}

#[test]
fn an_agent_that_weakens_the_contract_after_read_is_an_incident() {
    let cases: [(fn(&mut Value), &str); 3] = [
        (|t| t["acceptance_criteria"] = json!([]), "criterion AC-a"),
        (|t| t["required_checks"] = json!([]), "required check test"),
        (
            |t| t["failure_modes"][0]["status"] = json!("accepted_risk"),
            "FM-001",
        ),
    ];
    for (edit, want) in cases {
        let o = agent_edits_task(edit);
        assert_eq!(o.result["status"], "failed");
        let incident = o.result["incident"].as_str().unwrap();
        assert!(
            incident.contains(want) && incident.contains("after work:a"),
            "{incident}"
        );
        assert!(!proc_labels(&o).iter().any(|l| l.starts_with("review")));
    }
}

#[test]
fn a_planted_approval_aborts_the_task() {
    let hx = Arc::new(fake_hx(&[unit("a", json!({}))], &[("test", "npm test")]));
    let h = hx.clone();
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        hx: Some(hx),
        answer: Arc::new(move |label, _| {
            if label == "challenge" {
                *h.approvals.lock().unwrap() = json!({ "ready_to_build": { "status": "approved", "by": "agent", "at": "now" } });
            }
            None
        }),
        ..Opts::default()
    });
    assert!(o.result["incident"]
        .as_str()
        .unwrap()
        .contains("became approved outside finalize"));
    assert!(!proc_labels(&o).iter().any(|l| l.starts_with("work")));
}

#[test]
fn a_guard_incident_aborts_the_task() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        guard_incident: Some("work:a"),
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "failed");
    assert!(o.result["incident"]
        .as_str()
        .unwrap()
        .contains("guard incident in work:a"));
    assert!(!o.calls.contains(&"check:L1".to_string()));
}

#[test]
fn an_unapproved_check_command_escalates_checks() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        approve_checks: false,
        ..Opts::default()
    });
    assert_eq!(o.calls, ["read", "challenge"]);
    assert_eq!(o.result["status"], "needs_decision");
    let e = &o.result["escalations"][0];
    assert_eq!(e["id"], "CHECKS");
    assert!(e["question"]
        .as_str()
        .unwrap()
        .contains("`test` = `npm test` (not a docker compose command"));
}

#[test]
fn a_reading_that_leaves_other_gate_blockers_gets_one_read_gate_call_then_fails() {
    let hx = Arc::new(fake_hx(&[unit("a", json!({}))], &[("test", "npm test")]));
    hx.blockers
        .lock()
        .unwrap()
        .push("Define a test plan with at least one concrete scenario.".into());
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        hx: Some(hx),
        ..Opts::default()
    });
    assert_eq!(proc_labels(&o), ["read", "read!gate"]);
    assert!(o.prompt("read!gate").contains("Define a test plan"));
    assert_eq!(o.result["status"], "failed");
    assert!(o.result["note"]
        .as_str()
        .unwrap()
        .contains("does not pass ready-to-build"));

    let hx = Arc::new(fake_hx(&[unit("a", json!({}))], &[("test", "npm test")]));
    hx.blockers
        .lock()
        .unwrap()
        .push("Define a test plan with at least one concrete scenario.".into());
    let h = hx.clone();
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        hx: Some(hx),
        answer: Arc::new(move |label, _| {
            if label == "read!gate" {
                h.blockers.lock().unwrap().truncate(1);
            }
            None
        }),
        ..Opts::default()
    });
    assert_eq!(
        o.result["status"], "ready_for_review",
        "the fixed Reading goes on"
    );
}

#[test]
fn every_required_check_already_failing_ends_needs_decision_instead_of_shipping_unverified() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        checker: Arc::new(|_, _, _| Some("broken before we started".into())),
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "needs_decision");
    assert_eq!(ids(&o.result["escalations"]), ["BASELINE"]);
    assert!(!o.calls.iter().any(|c| c.starts_with("work")));
}

#[test]
fn the_call_cap_parks_the_task_with_its_edits_kept() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        max_calls: 3,
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "parked");
    assert_eq!(o.result["note"], "call cap");
    assert_eq!(proc_labels(&o), ["read", "challenge", "work:a"]);
}
