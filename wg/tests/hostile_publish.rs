//! Hostile tests for a shift's publish (design §4.1, §9.4, §15.3): every refusal the kernel makes
//! on its own tokens at the current tree, incidents at publish, a rerun that must not shrink the
//! gate, and the `max_parallel_units` cap.

mod support;

use serde_json::json;
use std::sync::{Arc, Mutex};
use support::*;
use wg_factory::api::{Command, Event, LlmCall};
use wg_factory::model::RunState;

fn real(state: &RunState, ev: &Event) -> (RunState, Vec<Command>) {
    wg_factory::engine::step(state, ev)
}

fn publish_refusals(o: &Out) -> Vec<String> {
    o.journal
        .iter()
        .filter_map(|e| e["refused"].as_str())
        .filter(|r| r.starts_with("publish: "))
        .map(str::to_string)
        .collect()
}

fn shift(o: Opts) -> Opts {
    let units = if o.units.is_empty() {
        vec![unit("a", json!({}))]
    } else {
        o.units.clone()
    };
    Opts {
        units,
        shift: true,
        ..o
    }
}

/// The honest run, with `before` applied just as the factory asks to publish.
fn at_publish(o: Opts, before: impl Fn(&mut Vec<Command>) + Send + Sync + 'static) -> Out {
    let step = move |s: &RunState, ev: &Event| {
        let (next, mut cmds) = real(s, ev);
        if cmds.iter().any(|c| matches!(c, Command::Publish { .. })) {
            before(&mut cmds);
        }
        (next, cmds)
    };
    run_with(shift(o), &step)
}

#[test]
fn an_edit_after_the_last_check_is_refused_at_publish() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let t = tree.clone();
    let o = at_publish(
        Opts {
            tree,
            ..Opts::default()
        },
        move |_| *t.lock().unwrap() = "tree-2".into(),
    );
    let r = publish_refusals(&o);
    assert!(
        r.iter()
            .any(|w| w.contains("a gate check has no passing result at the current tree")),
        "{r:?}"
    );
    assert_eq!(o.result["status"], "stuck");
}

#[test]
fn a_planted_approval_or_a_removed_criterion_at_publish_is_an_incident() {
    type Plant = fn(&FakeHx);
    let cases: [(Plant, &str); 2] = [
        (
            |h| {
                *h.approvals.lock().unwrap() = json!({ "ready_to_build": { "status": "approved", "by": "agent", "at": "now" } })
            },
            "became approved outside finalize",
        ),
        (
            |h| h.task.lock().unwrap()["acceptance_criteria"] = json!([]),
            "criterion AC-a",
        ),
    ];
    for (plant, want) in cases {
        let hx = Arc::new(fake_hx(&[unit("a", json!({}))], &[("test", "npm test")]));
        let h = hx.clone();
        let o = at_publish(
            Opts {
                hx: Some(hx),
                ..Opts::default()
            },
            move |_| plant(&h),
        );
        let incident = o.result["incident"].as_str().unwrap_or_default();
        assert!(incident.contains(want), "{want}: {}", o.result);
        assert_eq!(o.result["status"], "failed", "an incident, never stuck");
        assert!(o
            .journal
            .iter()
            .any(|e| e["kind"] == "incident" && e["at"] == "publish"));
        assert!(publish_refusals(&o).is_empty());
    }
}

#[test]
fn a_hundred_character_title_is_refused_and_never_becomes_a_path() {
    let long = format!("feat: {}", "a".repeat(100));
    let title = long.clone();
    let o = at_publish(Opts::default(), move |cmds| {
        for c in cmds.iter_mut() {
            if let Command::Publish { title: t } = c {
                *t = title.clone();
            }
        }
    });
    let r = publish_refusals(&o);
    assert!(
        r.iter()
            .any(|w| w.contains("not one conventional-commit line")),
        "{r:?}"
    );
    let logs = o.run_dir.join("logs");
    assert!(walk(&o.home)
        .iter()
        .filter(|p| !p.starts_with(&logs))
        .all(|p| !p.to_string_lossy().contains(&"a".repeat(40))));
}

#[test]
fn an_escalation_raised_after_the_review_is_refused_at_publish() {
    // The factory asks a second Challenger after the review, whose answer opens an escalation,
    // then asks to publish anyway.
    let first: Arc<Mutex<Option<LlmCall>>> = Arc::new(Mutex::new(None));
    let f = first.clone();
    let step = move |s: &RunState, ev: &Event| {
        if matches!(ev, Event::LlmDone { label, .. } if label == "challenge!retry") {
            let title = "feat: add a thing".to_string();
            return (s.clone(), vec![Command::Publish { title }]);
        }
        let (next, cmds) = real(s, ev);
        let cmds = cmds
            .into_iter()
            .map(|c| match c {
                Command::Llm(call) if call.label == "challenge" => {
                    *f.lock().unwrap() = Some(call.clone());
                    Command::Llm(call)
                }
                Command::Publish { .. } => {
                    let again = f.lock().unwrap().clone().expect("a challenge ran");
                    Command::Llm(LlmCall {
                        label: "challenge!retry".into(),
                        ..again
                    })
                }
                c => c,
            })
            .collect();
        (next, cmds)
    };
    let o = run_with(
        shift(Opts {
            answer: Arc::new(|label, _| {
                (label == "challenge!retry").then(|| json!({ "added": [], "assign": [], "escalations": [
                    { "id": "E-LATE", "severity": "high", "question": "Ship it?", "recommendation": "No", "options": [] }] }))
            }),
            ..Opts::default()
        }),
        &step,
    );
    let r = publish_refusals(&o);
    assert!(
        r.iter().any(|w| w.contains("open escalations: E-LATE")),
        "{r:?}"
    );
}

#[test]
fn a_rerun_never_drops_a_check_the_first_runs_edits_broke() {
    // The first run's clean baseline passed `test`; its edits broke it and it parked. The rerun's
    // baseline on the kept worktree sees `test` fail, yet `test` stays in the gate.
    let o = run_with(
        shift(Opts {
            checker: Arc::new(|_, _, _| Some("FAIL broken by run 1".to_string())),
            baseline_floor: Some(vec![]),
            ..Opts::default()
        }),
        &real,
    );
    assert_eq!(o.result["checks"], json!(["test"]), "{}", o.result);
    assert_eq!(o.result["checksAlreadyFailing"], json!([]));
    assert_eq!(o.result["status"], "stuck");
    assert!(!o.journal.iter().any(|e| e["what"] == "publish"));

    // A check that also failed at the first clean baseline may still leave the gate.
    let o = run_with(
        shift(Opts {
            checks: vec![("test", "npm test"), ("lint", "npm run lint")],
            checker: Arc::new(|_, cmd, _| (cmd == "npm run lint").then(|| "lint is red".into())),
            baseline_floor: Some(vec!["lint"]),
            ..Opts::default()
        }),
        &real,
    );
    assert_eq!(o.result["checks"], json!(["test"]), "{}", o.result);
    assert_eq!(o.result["checksAlreadyFailing"], json!(["lint"]));
}

#[test]
fn max_parallel_units_caps_concurrent_workers() {
    let units = || {
        vec![
            unit("a", json!({})),
            unit("b", json!({})),
            unit("c", json!({})),
        ]
    };
    for (cap, want) in [(2, 2), (1, 1)] {
        let o = run_with(
            shift(Opts {
                units: units(),
                max_parallel: cap,
                work_ms: 200,
                ..Opts::default()
            }),
            &real,
        );
        let workers = o
            .proc_calls
            .iter()
            .filter(|c| c.label.starts_with("work:"))
            .count();
        assert_eq!(workers, 3, "{}", o.result);
        assert_eq!(o.max_in_flight, want, "cap {cap}");
    }
}
