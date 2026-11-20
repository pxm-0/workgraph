//! Sandbox flows through the real driver and factory with fake ports (design §8.1-§8.2, §15.2):
//! a unit that turns risky after it was built, the agent dying mid-task, a shift whose risky work
//! cannot run, and a hostile factory asking for a risky Worker on its own terms. Never the Mac.

mod support;

use serde_json::{json, Value};
use std::collections::HashMap;
use support::*;
use wg_factory::api::{Command, Event, LlmCall, Role, SchemaId, Tier};
use wg_factory::model::RunState;

fn host_checks_after_baseline(o: &Out) -> Vec<String> {
    o.journal
        .iter()
        .filter(|e| {
            e["kind"] == "checks_run" && e["label"] != "check:baseline" && e["target"] == "host"
        })
        .map(|e| e["label"].as_str().unwrap().to_string())
        .collect()
}

fn diffs() -> HashMap<&'static str, &'static str> {
    HashMap::from([(
        "check:baseline",
        "+++ b/src/c.js\n+db.query('DROP TABLE users')\n",
    )])
}

#[test]
fn a_unit_whose_diff_turns_risky_has_its_checks_rerun_in_the_sandbox() {
    let o = run(Opts {
        units: vec![unit("c", json!({}))],
        changed: HashMap::from([("check:baseline", vec!["src/c.js"])]),
        diffs: diffs(),
        sandbox: Some(SandboxSpec::default()),
        config_extra: sandbox_repo(),
        ..Opts::default()
    });
    assert_eq!(o.result["tainted"], true);
    assert_eq!(o.result["riskyBuilt"][0]["id"], "c");
    assert_eq!(o.result["riskyBuilt"][0]["label"], "risky:migration");
    assert!(
        host_checks_after_baseline(&o).is_empty(),
        "every check after the layer ran in the sandbox"
    );
    assert!(o
        .journal
        .iter()
        .any(|e| e["kind"] == "risk_updated" && e["after_the_fact"].is_array()));
}

#[test]
fn without_a_sandbox_a_unit_that_turns_risky_parks_the_task() {
    let o = run(Opts {
        units: vec![unit("c", json!({}))],
        changed: HashMap::from([("check:baseline", vec!["src/c.js"])]),
        diffs: diffs(),
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "parked");
    let note = o.result["note"].as_str().unwrap();
    assert!(
        note.starts_with("sandbox unavailable") && note.contains("turned out risky"),
        "{note}"
    );
    assert!(
        host_checks_after_baseline(&o).is_empty(),
        "never on the Mac"
    );
    assert!(!o.calls.iter().any(|c| c.starts_with("review")));
}

#[test]
fn the_agent_dying_mid_task_parks_it_and_nothing_runs_on_the_mac() {
    let o = run(Opts {
        units: vec![
            unit(
                "a",
                json!({ "risky": true, "riskReason": "auth", "files": ["src/auth/"] }),
            ),
            unit("b", json!({ "dependsOn": ["a"] })),
        ],
        sandbox: Some(SandboxSpec {
            unavailable: None,
            dies_at: Some("check:L1"),
        }),
        config_extra: sandbox_repo(),
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "parked");
    assert!(
        o.result["note"]
            .as_str()
            .unwrap()
            .contains("sandbox unavailable"),
        "{}",
        o.result
    );
    assert!(host_checks_after_baseline(&o).is_empty());
    assert!(
        !o.calls.contains(&"work:b".to_string()),
        "nothing after the agent died"
    );
}

#[test]
fn in_a_shift_risky_work_without_a_ready_sandbox_parks_before_any_edit() {
    let o = run(Opts {
        units: vec![
            unit("a", json!({ "risky": true, "riskReason": "migration" })),
            unit("c", json!({})),
        ],
        sandbox: Some(SandboxSpec {
            unavailable: Some("container_isolation_failed:network: interfaces \"lo eth0\""),
            dies_at: None,
        }),
        config_extra: sandbox_repo(),
        shift: true,
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "parked");
    assert!(o.result["note"]
        .as_str()
        .unwrap()
        .contains("container_isolation_failed:network"));
    assert!(
        !o.calls.iter().any(|c| c.starts_with("work:")),
        "{:?}",
        o.calls
    );
}

#[test]
fn an_always_tainted_repo_runs_even_its_baseline_in_the_sandbox() {
    let o = run(Opts {
        units: vec![unit("c", json!({}))],
        sandbox: Some(SandboxSpec::default()),
        config_extra: sandbox_repo()
            .replace("path = \"/x\"\n", "path = \"/x\"\nalways_tainted = true\n"),
        ..Opts::default()
    });
    let targets: Vec<&Value> = o
        .journal
        .iter()
        .filter(|e| e["kind"] == "checks_run")
        .map(|e| &e["target"])
        .collect();
    assert!(
        !targets.is_empty() && targets.iter().all(|t| *t == "sim_sandbox"),
        "{targets:?}"
    );
    assert_eq!(o.result["verified"], true);
}

/// A factory that skips straight to a Worker for the risky unit, with its own tier and no grant.
fn hostile(state: &RunState, ev: &Event) -> (RunState, Vec<Command>) {
    let (next, cmds) = wg_factory::engine::step(state, ev);
    let cmds = cmds
        .into_iter()
        .map(|c| match c {
            Command::Llm(call) if call.label == "work:b" => Command::Llm(LlmCall {
                label: "work:a".into(),
                role: Role::Worker,
                unit: Some("a".into()),
                tier: Tier::Haiku,
                schema: SchemaId::Work,
                ..call
            }),
            c => c,
        })
        .collect();
    (next, cmds)
}

#[test]
fn a_hostile_factory_cannot_give_a_risky_worker_host_checks() {
    let o = run_with(
        Opts {
            units: vec![
                unit("a", json!({ "risky": true, "riskReason": "secrets" })),
                unit("b", json!({})),
            ],
            sandbox: Some(SandboxSpec::default()),
            config_extra: sandbox_repo(),
            ..Opts::default()
        },
        &hostile,
    );
    for c in o.proc_calls.iter().filter(|c| c.label == "work:a") {
        let argv = c.argv.join(" ");
        assert!(argv.contains("sandbox exec --unit a:*"), "{argv}");
        assert!(
            !argv.contains("Bash(npm test:*)"),
            "the kernel, not the factory, picks the policy"
        );
    }
    let without = run_with(
        Opts {
            units: vec![
                unit("a", json!({ "risky": true, "riskReason": "secrets" })),
                unit("b", json!({})),
            ],
            ..Opts::default()
        },
        &hostile,
    );
    assert!(
        !without.proc_calls.iter().any(|c| c.label == "work:a"),
        "no sandbox: refused"
    );
}

fn targets(o: &Out) -> Vec<(String, String)> {
    o.journal
        .iter()
        .filter(|e| e["kind"] == "checks_run")
        .map(|e| {
            (
                e["label"].as_str().unwrap().to_string(),
                e["target"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// A rerun of a task an earlier run tainted (its kept worktree holds code built in the sandbox):
/// even the baseline runs there, and without a sandbox the task parks (design §8.1).
#[test]
fn a_task_tainted_by_an_earlier_run_stays_tainted_from_its_baseline() {
    let o = run(Opts {
        units: vec![unit("c", json!({}))],
        tainted_earlier: true,
        sandbox: Some(SandboxSpec::default()),
        config_extra: sandbox_repo(),
        ..Opts::default()
    });
    let t = targets(&o);
    assert_eq!(t[0], ("check:baseline".into(), "sim_sandbox".into()));
    assert!(t.iter().all(|(_, x)| x == "sim_sandbox"), "{t:?}");
    let parked = run(Opts {
        units: vec![unit("c", json!({}))],
        tainted_earlier: true,
        ..Opts::default()
    });
    assert_eq!(parked.result["status"], "parked");
    assert!(targets(&parked).is_empty(), "nothing on the Mac");
}

/// A risky unit's files already changed before the baseline (a kept worktree, whatever the earlier
/// runs recorded): the task is tainted before its baseline runs.
#[test]
fn a_risky_units_leftover_edits_taint_the_task_before_the_baseline() {
    let o = run(Opts {
        units: vec![unit(
            "a",
            json!({ "risky": true, "riskReason": "migration", "files": ["db/migrations/"] }),
        )],
        changed: HashMap::from([("", vec!["db/migrations/1.sql"])]),
        sandbox: Some(SandboxSpec::default()),
        config_extra: sandbox_repo(),
        ..Opts::default()
    });
    assert_eq!(
        targets(&o)[0],
        ("check:baseline".into(), "sim_sandbox".into())
    );
    assert_eq!(o.result["tainted"], true);
}

/// A plain unit checked before a risky one of the same layer: the layer runs at once in one
/// worktree, so the plain Worker gets no host check either (design §8.1).
#[test]
fn a_plain_unit_beside_a_risky_one_in_a_layer_gets_no_host_bash() {
    let o = run(Opts {
        units: vec![
            unit("c", json!({})),
            unit(
                "a",
                json!({ "risky": true, "riskReason": "migration", "files": ["db/migrations/"] }),
            ),
        ],
        sandbox: Some(SandboxSpec::default()),
        config_extra: sandbox_repo(),
        ..Opts::default()
    });
    let argv = |label: &str| o.call(label).argv.join(" ");
    assert!(
        !argv("work:c").contains("Bash(npm test:*)"),
        "{}",
        argv("work:c")
    );
    assert!(argv("work:c").contains("sandbox exec --unit c:*"));
    assert!(argv("work:a").contains("sandbox exec --unit a:*"));
}
