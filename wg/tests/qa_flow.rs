//! QA and release pipelines end to end with fakes (design §6.2, §16 Phase 4): a seeded e2e failure
//! fixed by one blamed round through the build fix loop, re-reviewed, then published; an explorer's
//! finding kept only when the kernel's sandbox rerun of its repro fails (§6.5), fixed and explored
//! again; a QA fix published without its re-review refused; a QA set the kernel requires at
//! publish whatever the factory does; a new stage added with TOML and a prompt only; and the release line stopping at the owner's gate only on the kernel's evidence.

mod support;

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use support::*;
use wg::engine::driver as driver_step;
use wg_factory::api::{Command, Event, LlmCall};
use wg_factory::model::RunState;

const E2E_FAIL: &str = "FAIL e2e/login.spec.js: src/b.js:7 keeps a stale session after logout";

fn labels(o: &Out) -> Vec<String> {
    o.proc_calls.iter().map(|c| c.label.clone()).collect()
}

fn check_labels(o: &Out) -> Vec<String> {
    o.journal
        .iter()
        .filter(|e| e["kind"] == "checks_run")
        .map(|e| e["label"].as_str().unwrap().to_string())
        .collect()
}

fn publish_refusals(o: &Out) -> Vec<String> {
    o.journal
        .iter()
        .filter_map(|e| e["refused"].as_str())
        .filter(|r| r.starts_with("publish: "))
        .map(str::to_string)
        .collect()
}

/// The `--allowedTools` entries of a call.
fn allowed(c: &Call) -> Vec<String> {
    let from = c.argv.iter().position(|a| a == "--allowedTools").unwrap() + 1;
    c.argv[from..]
        .iter()
        .take_while(|a| !a.starts_with("--"))
        .cloned()
        .collect()
}

fn pos(v: &[String], l: &str) -> usize {
    v.iter()
        .position(|x| x == l)
        .unwrap_or_else(|| panic!("{l} missing from {v:?}"))
}

/// Two units, the `e2e` QA check, a shift; `fixed_by` changes the tree when it runs.
fn qa(checker: Checker, fixed_by: &'static [&'static str], tree: Arc<Mutex<String>>) -> Opts {
    let t = tree.clone();
    Opts {
        units: vec![unit("a", json!({})), unit("b", json!({}))],
        qa_checks: vec![("e2e", "npm run e2e")],
        shift: true,
        checker,
        tree,
        answer: Arc::new(move |label, _| {
            if fixed_by.contains(&label) {
                *t.lock().unwrap() = format!("tree-after-{label}");
            }
            None
        }),
        ..Opts::default()
    }
}

fn e2e_fails_once() -> Checker {
    Arc::new(|label, cmd, _| {
        (label == "check:regression" && cmd == "npm run e2e").then(|| E2E_FAIL.to_string())
    })
}

const REPRO: &str = "node -e 'require(\"./src/a\")(\"\")'";

/// A finding as the explorer reports it, with its repro.
fn finding(id: &str, cmd: &str) -> Value {
    json!({ "id": id, "severity": "high", "file": "src/a.js", "summary": "empty input crashes",
            "repro": { "cmd": cmd, "output": "TypeError: x.trim is not a function", "expected": "exit 0, empty result" } })
}

/// `qa` with a ready sandbox, where the kernel reruns the explorer's repros.
fn qa_sandboxed(checker: Checker, tree: Arc<Mutex<String>>) -> Opts {
    Opts {
        sandbox: Some(SandboxSpec::default()),
        config_extra: sandbox_repo(),
        ..qa(checker, &[], tree)
    }
}

/// The repro `cmd` fails when the kernel reruns it after the explorer call `explore`.
fn repro_fails(cmd: &'static str) -> Checker {
    Arc::new(move |label, c, _| {
        (label.starts_with("explore.repro") && c == cmd).then(|| "TypeError".to_string())
    })
}

fn dropped(o: &Out) -> Vec<(String, String)> {
    o.journal
        .iter()
        .filter(|e| e["kind"] == "qa_finding_dropped")
        .map(|e| {
            (
                e["id"].as_str().unwrap().to_string(),
                e["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn a_seeded_e2e_failure_in_qa_is_fixed_by_one_blamed_round_re_reviewed_then_published() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let o = run(qa(e2e_fails_once(), &["regression:b#1"], tree));
    let l = labels(&o);
    // Dev first, untouched by QA: its review, then QA's stages in chain order.
    assert!(pos(&l, "review#1") < pos(&l, "regression:b#1"));
    assert!(
        !l.contains(&"regression:a#1".to_string()),
        "the failure names src/b.js, so only b is blamed: {l:?}"
    );
    assert!(pos(&l, "regression:b#1") < pos(&l, "explore"));
    assert!(pos(&l, "explore") < pos(&l, "rereview#1"));
    // The fix goes through the build fix loop: the fix prompt, the failing output, the fixer role.
    let fix = o.call("regression:b#1");
    assert!(fix.prompt.contains("This is fix round 1 of 1"));
    assert!(fix.prompt.contains(E2E_FAIL));
    assert!(o
        .prompt("rereview#1")
        .contains("This is a re-review after fixes"));
    let checks = check_labels(&o);
    assert_eq!(
        &checks[checks.len() - 2..],
        ["check:regression", "check:regression#1"]
    );
    // The QA set runs with the gate: both checks, every time.
    let qa_run = o
        .journal
        .iter()
        .find(|e| e["kind"] == "checks_run" && e["label"] == "check:regression")
        .unwrap();
    assert_eq!(
        qa_run["results"],
        json!([{ "name": "test", "exit": 0 }, { "name": "e2e", "exit": 1 }])
    );
    // Published: every invariant held at the fixed tree (the fake repo fails only at git).
    let r = publish_refusals(&o);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].contains("git branch"), "past the invariants: {r:?}");
    assert_eq!(o.result["reviewed"], true, "{}", o.result);
    assert_eq!(o.result["verified"], true);
    assert_eq!(o.result["fixRounds"], 1);
}

#[test]
fn qa_that_still_fails_after_its_round_leaves_the_task_stuck_and_unpublished() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let always: Checker = Arc::new(|label, cmd, _| {
        (label.starts_with("check:regression") && cmd == "npm run e2e").then(|| E2E_FAIL.into())
    });
    let o = run(qa(always, &[], tree));
    assert_eq!(o.result["status"], "stuck");
    assert!(o.result["note"]
        .as_str()
        .unwrap()
        .starts_with("stage regression: checks failing: npm run e2e"));
    assert_eq!(ids(&o.result["stuck"]), ["b"]);
    assert!(!o.journal.iter().any(|e| e["what"] == "publish"));
    assert!(!labels(&o).contains(&"explore".to_string()));
}

#[test]
fn an_explorers_proven_finding_gets_one_fix_round_then_explore_runs_again() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let mut o = qa_sandboxed(repro_fails(REPRO), tree.clone());
    let t = tree.clone();
    o.answer = Arc::new(move |label, _| match label {
        "explore" => Some(json!({ "passed": false, "findings": [finding("QA-1", REPRO)] })),
        "explore:a#1" => {
            *t.lock().unwrap() = "tree-2".into();
            None
        }
        _ => None,
    });
    let o = run(o);
    let l = labels(&o);
    assert!(pos(&l, "explore") < pos(&l, "explore:a#1"));
    assert!(pos(&l, "explore:a#1") < pos(&l, "explore#2"));
    assert!(pos(&l, "explore#2") < pos(&l, "rereview#1"));
    // The fixer sees the finding with the repro the kernel reran.
    let fix = o.prompt("explore:a#1");
    assert!(
        fix.contains("QA-1 [high] src/a.js: empty input crashes"),
        "{fix}"
    );
    assert!(
        fix.contains(&format!(
            "repro (the kernel reran it and it failed): {REPRO}"
        )),
        "{fix}"
    );
    assert!(fix.contains("expected: exit 0, empty result"), "{fix}");
    assert!(o.prompt("explore#2").contains("This is run 2"));
    assert!(o.prompt("explore#2").contains("QA-1"));
    assert!(check_labels(&o).contains(&"check:explore#1".to_string()));
    // The repro ran in the sandbox, not on the Mac, and was kept.
    let rerun = o
        .journal
        .iter()
        .find(|e| e["kind"] == "sandbox_run" && e["what"] == "qa repro")
        .unwrap();
    assert_eq!(
        rerun["steps"],
        json!([{ "name": "repro-1", "exit": 1, "timed_out": false, "oom": false }])
    );
    assert!(dropped(&o).is_empty());
    // The explorer is read-only but for the run's tmp; with a sandbox it runs commands only there.
    let call = o.call("explore");
    assert_eq!(call.model, "sonnet");
    let tools = allowed(call);
    assert!(
        tools
            .iter()
            .any(|t| t.contains("sandbox exec --task T-001")),
        "{tools:?}"
    );
    assert!(
        !tools.iter().any(|t| t.contains("npm run e2e")),
        "{tools:?}"
    );
    assert!(!tools.iter().any(|t| t.contains("src/a.js")), "{tools:?}");
    assert!(publish_refusals(&o)[0].contains("git branch"));
}

#[test]
fn an_honest_qa_with_no_fix_makes_no_re_review_call() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let o = run(qa(Arc::new(|_, _, _| None), &[], tree));
    let l = labels(&o);
    assert!(l.contains(&"explore".to_string()));
    assert!(!l.iter().any(|x| x.starts_with("rereview")), "{l:?}");
    assert!(publish_refusals(&o)[0].contains("git branch"));
}

#[test]
fn an_explore_fix_that_breaks_e2e_is_caught_by_the_qa_set_and_never_published() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let repro = repro_fails(REPRO);
    let mut o = qa_sandboxed(
        Arc::new(move |label, cmd, n| {
            ((label == "check:explore#1" && cmd == "npm run e2e").then(|| E2E_FAIL.into()))
                .or_else(|| repro(label, cmd, n))
        }),
        tree.clone(),
    );
    let t = tree.clone();
    o.answer = Arc::new(move |label, _| match label {
        "explore" => Some(json!({ "passed": false, "findings": [finding("QA-1", REPRO)] })),
        "explore:a#1" => {
            *t.lock().unwrap() = "tree-2".into();
            None
        }
        _ => None,
    });
    let o = run(o);
    // The factory asked for no check names; the kernel ran the QA set anyway.
    let recheck = o
        .journal
        .iter()
        .find(|e| e["kind"] == "checks_run" && e["label"] == "check:explore#1")
        .unwrap();
    assert_eq!(
        recheck["results"],
        json!([{ "name": "test", "exit": 0 }, { "name": "e2e", "exit": 1 }])
    );
    assert_eq!(o.result["status"], "stuck", "{}", o.result);
    assert!(o.result["note"]
        .as_str()
        .unwrap()
        .contains("checks fail after its fix round: npm run e2e"));
    assert!(!o.journal.iter().any(|e| e["what"] == "publish"));
}

#[test]
fn publish_needs_the_qa_set_passing_whatever_the_factory_skips() {
    // Skips `regression`: asks to publish where its QA checks were due.
    let skip = |s: &RunState, ev: &Event| {
        let (next, cmds) = wg_factory::engine::step(s, ev);
        let cmds = cmds
            .into_iter()
            .map(|c| match c {
                Command::Checks { label, .. } if label == "check:regression" => Command::Publish {
                    title: "feat: skip the e2e suite".into(),
                },
                c => c,
            })
            .collect();
        (next, cmds)
    };
    // Publishes after `regression` failed instead of fixing it.
    let ignore = |s: &RunState, ev: &Event| {
        let (next, cmds) = wg_factory::engine::step(s, ev);
        let cmds = cmds
            .into_iter()
            .map(|c| match c {
                Command::Llm(LlmCall { label, .. }) if label.starts_with("regression:") => {
                    Command::Publish {
                        title: "feat: ship it anyway".into(),
                    }
                }
                c => c,
            })
            .collect();
        (next, cmds)
    };
    for (step, checker) in [
        (
            &skip as driver_step::Step,
            Arc::new(|_: &str, _: &str, _| None) as Checker,
        ),
        (&ignore as driver_step::Step, e2e_fails_once()),
    ] {
        let tree = Arc::new(Mutex::new("tree-1".to_string()));
        let o = run_with(qa(checker, &[], tree), step);
        let r = publish_refusals(&o);
        assert!(
            r.iter()
                .any(|w| w.contains("a QA check has no passing result at the current tree")),
            "{r:?}"
        );
        assert!(o.result["pr"].is_null());
    }
}

#[test]
fn a_qa_fix_after_review_published_without_its_re_review_is_refused() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    // A hostile factory: when the re-review is due, it asks to publish instead.
    let step = |s: &RunState, ev: &Event| {
        let (next, cmds) = wg_factory::engine::step(s, ev);
        let cmds = cmds
            .into_iter()
            .map(|c| match c {
                Command::Llm(LlmCall { label, .. }) if label == "rereview#1" => Command::Publish {
                    title: "feat: skip the re-review".into(),
                },
                c => c,
            })
            .collect();
        (next, cmds)
    };
    let o = run_with(qa(e2e_fails_once(), &["regression:b#1"], tree), &step);
    let r = publish_refusals(&o);
    assert!(
        r.iter()
            .any(|w| w.contains("no independent opus review passed at the current tree")),
        "{r:?}"
    );
    // The factory is left waiting for a re-review it never asked for: no PR, not ready.
    assert!(o.result["pr"].is_null());
    assert_ne!(o.result["status"], "ready_for_review");
    assert_eq!(o.result["reviewed"], false);
}

#[test]
fn a_new_stage_needs_only_toml_and_a_prompt() {
    let dir = temp("new-stage");
    std::fs::create_dir_all(dir.join("prompts")).unwrap();
    std::fs::create_dir_all(dir.join("pipelines")).unwrap();
    std::fs::write(
        dir.join("prompts/security-scan.md"),
        "Scan the change for {{task}} ({{units}}) for leaked secrets. Report each as a finding.\n",
    )
    .unwrap();
    let shipped = include_str!("../../pipelines/qa.toml");
    let added = "[[stage]]\nid = \"security-scan\"\nkind = \"agent\"\nrole = \"qa-explorer\"\nmodel = \"haiku\"\nprompt = \"security-scan\"\nschema = \"verdict\"\ngate = { field = \"passed\", equals = true }\n\n[[stage]]                    # required";
    let qa_toml = shipped.replacen("[[stage]]                    # required", added, 1);
    assert_ne!(qa_toml, shipped);
    std::fs::write(dir.join("pipelines/qa.toml"), qa_toml).unwrap();
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let mut o = qa(Arc::new(|_, _, _| None), &[], tree);
    o.overlay = Some(dir);
    o.answer = Arc::new(|label, _| {
        (label == "securityscan").then(|| json!({ "passed": true, "findings": [] }))
    });
    let o = run(o);
    let l = labels(&o);
    assert!(pos(&l, "explore") < pos(&l, "securityscan"), "{l:?}");
    let call = o.call("securityscan");
    assert_eq!(call.model, "haiku");
    assert!(call
        .prompt
        .contains("Scan the change for add a thing (a (src/a.js); b (src/b.js))"));
    assert!(publish_refusals(&o)[0].contains("git branch"));
}

#[test]
fn a_finding_whose_rerun_does_not_fail_is_dropped_with_its_reason() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let mut o = qa_sandboxed(repro_fails(REPRO), tree.clone());
    let t = tree.clone();
    o.answer = Arc::new(move |label, _| match label {
        // QA-2's repro passes on the kernel's rerun: a claim, not a proof.
        "explore" => Some(json!({ "passed": false, "findings": [
            finding("QA-1", REPRO), finding("QA-2", "node -e 'process.exit(0)'")] })),
        "explore:a#1" => {
            *t.lock().unwrap() = "tree-2".into();
            None
        }
        _ => None,
    });
    let o = run(o);
    assert_eq!(
        dropped(&o),
        [(
            "QA-2".to_string(),
            "the kernel's rerun of the repro exited 0".to_string()
        )]
    );
    let fix = o.prompt("explore:a#1");
    assert!(fix.contains("QA-1") && !fix.contains("QA-2"), "{fix}");
}

#[test]
fn unproven_findings_alone_pass_qa_and_send_nothing_back() {
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let mut o = qa_sandboxed(Arc::new(|_, _, _| None), tree);
    o.answer = Arc::new(|label, _| {
        (label == "explore")
            .then(|| json!({ "passed": false, "findings": [finding("QA-1", "exit 0")] }))
    });
    let o = run(o);
    assert_eq!(dropped(&o)[0].0, "QA-1");
    let l = labels(&o);
    assert!(
        !l.iter().any(|x| x.starts_with("explore:")
            || x.starts_with("explore#")
            || x.starts_with("rereview")),
        "{l:?}"
    );
    assert!(publish_refusals(&o)[0].contains("git branch"));
}

#[test]
fn without_a_sandbox_no_repro_runs_and_every_finding_is_dropped() {
    // The Mac runs only approved command strings: a repro never runs there, even untainted.
    let tree = Arc::new(Mutex::new("tree-1".to_string()));
    let mut o = qa(Arc::new(|_, _, _| None), &[], tree);
    o.answer = Arc::new(|label, _| {
        (label == "explore")
            .then(|| json!({ "passed": false, "findings": [finding("QA-1", REPRO)] }))
    });
    let o = run(o);
    let d = dropped(&o);
    assert_eq!(d[0].0, "QA-1");
    assert!(d[0].1.starts_with("sandbox unavailable"), "{d:?}");
    assert!(!o.journal.iter().any(|e| e["what"] == "qa repro"));
    // Untainted and without a sandbox, the explorer keeps the approved checks.
    assert!(allowed(o.call("explore")).contains(&"Bash(npm run e2e:*)".to_string()));
    assert!(!labels(&o).iter().any(|x| x.starts_with("explore:")));
}

/// The release line for T-001 with a three-step rehearsal; `tweak` changes the scenario.
fn release(tweak: impl FnOnce(&mut Opts)) -> Out {
    release_with(tweak, &wg_factory::engine::step)
}

fn release_with(tweak: impl FnOnce(&mut Opts), step: driver_step::Step) -> Out {
    let rehearsal = "release_rehearsal = [\"npm run migrate:up\", \"npm run migrate:down\", \"npm run migrate:up\"]\n";
    let mut o = Opts {
        units: vec![unit("a", json!({}))],
        pipelines: vec!["release"],
        resume_task: Some("T-001"),
        config_extra: format!("{}{rehearsal}", sandbox_repo()),
        sandbox: Some(SandboxSpec::default()),
        ..Opts::default()
    };
    tweak(&mut o);
    run_with(o, step)
}

fn sandbox_steps(o: &Out) -> Vec<Value> {
    o.journal
        .iter()
        .filter(|e| e["kind"] == "sandbox_run")
        .map(|e| e["steps"].clone())
        .collect()
}

#[test]
fn the_release_line_prepares_rehearses_in_the_sandbox_and_stops_at_the_owners_gate() {
    let o = release(|_| {});
    assert_eq!(labels(&o), ["notes", "plan"]);
    assert_eq!((o.model("notes"), o.model("plan")), ("haiku", "opus"));
    assert_eq!(o.result["status"], "awaiting_owner", "{}", o.result);
    assert_eq!(o.result["gate"], "ready-to-release");
    assert!(o.result["next"].as_str().unwrap().contains("never deploys"));
    // The rehearsal ran in the sandbox, step by step; nothing ran on the Mac.
    assert_eq!(
        sandbox_steps(&o),
        [
            json!([{ "name": "release-rehearsal-1", "exit": 0, "timed_out": false, "oom": false },
                { "name": "release-rehearsal-2", "exit": 0, "timed_out": false, "oom": false },
                { "name": "release-rehearsal-3", "exit": 0, "timed_out": false, "oom": false }])
        ]
    );
    assert!(check_labels(&o).is_empty());
    assert_eq!(o.result["sandboxRuns"][0]["label"], "rehearse");
    let gate = o
        .journal
        .iter()
        .find(|e| e["kind"] == "gate_evaluated")
        .unwrap();
    assert_eq!(gate["gate"], "ready-to-release");
    assert!(std::fs::read_to_string(o.run_dir.join("release-notes.md"))
        .unwrap()
        .contains("- add(2, 3) is 5"));
    // A release writer has no Bash and no Skill; only the plan may edit, and only the task file.
    for label in ["notes", "plan"] {
        let tools = allowed(o.call(label)).join(" ");
        assert!(
            !tools.contains("Bash") && !tools.contains("Skill"),
            "{label}: {tools}"
        );
        assert_eq!(
            tools.contains("T-001.json"),
            label == "plan",
            "{label}: {tools}"
        );
    }
    assert!(!o.journal.iter().any(|e| e["what"] == "publish"));
}

#[test]
fn a_release_without_a_sandbox_parks_and_never_rehearses_on_the_mac() {
    let o = release(|o| {
        o.sandbox = Some(SandboxSpec {
            unavailable: Some("doctor: container_isolation_failed:mounts"),
            ..SandboxSpec::default()
        })
    });
    assert_eq!(o.result["status"], "parked");
    assert!(o.result["note"]
        .as_str()
        .unwrap()
        .contains("sandbox unavailable: doctor: container_isolation_failed:mounts"));
    assert!(sandbox_steps(&o).is_empty() && check_labels(&o).is_empty());
    assert!(!o.journal.iter().any(|e| e["kind"] == "gate_evaluated"));
}

#[test]
fn a_failed_rehearsal_or_an_unexpected_gate_blocker_stops_before_the_owner() {
    let down: Checker = Arc::new(|label, cmd, _| {
        (label == "rehearse" && cmd == "npm run migrate:down")
            .then(|| "relation still exists".into())
    });
    let o = release(|o| o.checker = down);
    assert_eq!(o.result["status"], "stuck");
    assert_eq!(
        o.result["note"],
        "stage rehearse: sandbox steps failed: release-rehearsal-2 (exit 1)"
    );
    assert!(!o.journal.iter().any(|e| e["kind"] == "gate_evaluated"));

    let o = release(|o| {
        o.release_blockers = vec![
            "Release plan needs rollback steps.",
            "Human approval for ready_to_release is pending.",
        ]
    });
    assert_eq!(o.result["status"], "stuck");
    assert!(o.result["note"]
        .as_str()
        .unwrap()
        .contains("ready-to-release has other blockers: Release plan needs rollback steps."));
}

#[test]
fn a_factory_cannot_run_a_gate_sandbox_or_human_stage_its_chain_does_not_declare() {
    for extra in [
        Command::Sandbox {
            label: "rehearse".into(),
            steps_from: "release_rehearsal".into(),
        },
        Command::Gate {
            label: "gate".into(),
            gate: "ready-to-release".into(),
        },
        Command::Human {
            gate: "ready-to-release".into(),
            ask: "approve?".into(),
        },
    ] {
        let step = move |s: &RunState, ev: &Event| {
            let (next, mut cmds) = wg_factory::engine::step(s, ev);
            if matches!(ev, Event::Started(_)) {
                cmds.insert(0, extra.clone());
            }
            (next, cmds)
        };
        let tree = Arc::new(Mutex::new("tree-1".to_string()));
        let o = run_with(qa(Arc::new(|_, _, _| None), &[], tree), &step);
        assert_eq!(o.result["status"], "failed", "{}", o.result);
        assert!(o.result["note"]
            .as_str()
            .unwrap()
            .contains("a stage this chain does not have"));
        assert!(labels(&o).is_empty(), "nothing ran");
        assert!(!o
            .journal
            .iter()
            .any(|e| e["kind"] == "sandbox_run" || e["kind"] == "gate_evaluated"));
    }
}

/// A release factory that emits `with` in place of the first command matching `at`.
fn hostile_release(
    at: fn(&Event, &Command) -> bool,
    with: Command,
    tweak: impl FnOnce(&mut Opts),
) -> Out {
    let step = move |s: &RunState, ev: &Event| {
        let (next, cmds) = wg_factory::engine::step(s, ev);
        let cmds = cmds
            .into_iter()
            .map(|c| if at(ev, &c) { with.clone() } else { c })
            .collect();
        (next, cmds)
    };
    release_with(tweak, &step)
}

fn human() -> Command {
    Command::Human {
        gate: "ready-to-release".into(),
        ask: "approve?".into(),
    }
}

#[test]
fn the_owner_is_asked_only_on_the_kernels_rehearsal_and_gate() {
    // Straight to the owner, before notes, plan, rehearsal or gate.
    let o = hostile_release(|ev, _| matches!(ev, Event::Started(_)), human(), |_| {});
    assert!(labels(&o).is_empty(), "{:?}", labels(&o));
    // Past the plan, the rehearsal skipped.
    let skipped = hostile_release(|_, c| matches!(c, Command::Sandbox { .. }), human(), |_| {});
    assert_eq!(labels(&skipped), ["notes", "plan"]);
    for o in [&o, &skipped] {
        assert_eq!(o.result["status"], "failed", "{}", o.result);
        assert!(
            o.result["note"]
                .as_str()
                .unwrap()
                .contains("stage rehearse has no passing sandbox run at the current tree"),
            "{}",
            o.result
        );
        assert!(!o.journal.iter().any(|e| e["kind"] == "parked"));
        assert!(sandbox_steps(o).is_empty());
    }
    // Rehearsed, the gate skipped while it still has another blocker: the kernel evaluates it.
    let o = hostile_release(
        |_, c| matches!(c, Command::Gate { .. }),
        human(),
        |o| {
            o.release_blockers = vec![
                "Release plan needs rollback steps.",
                "Human approval for ready_to_release is pending.",
            ]
        },
    );
    assert_eq!(o.result["status"], "failed", "{}", o.result);
    assert!(o.result["note"].as_str().unwrap().contains(
        "ready-to-release has blockers besides the owner's approval: Release plan needs rollback steps."
    ));
    assert!(!o.journal.iter().any(|e| e["kind"] == "parked"));
}

#[test]
fn a_release_writer_incident_ends_the_run_as_an_incident() {
    let o = release(|o| o.guard_incident = Some("plan"));
    assert!(
        o.result["incident"]
            .as_str()
            .is_some_and(|i| i.starts_with("guard incident in plan")),
        "{}",
        o.result
    );
    assert_eq!(labels(&o), ["notes", "plan"]);
    assert!(sandbox_steps(&o).is_empty());
    assert!(!o.journal.iter().any(|e| e["kind"] == "parked"));
}
