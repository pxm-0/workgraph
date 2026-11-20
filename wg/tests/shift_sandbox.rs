//! A risky unit through a whole shift (design §16 Phase 3 exit, in CI form): the doctor at shift
//! start, the risky worker's only Bash being `wg sandbox exec`, every later check in the sandbox,
//! a draft PR labelled `risky:auth` carrying the SandboxRun evidence, the argus fingerprint before
//! and after, and `wg approve` verifying through the sandbox. A fake `orb` plays the agent and runs
//! the steps locally; a second shift with the agent down parks the task instead of using the Mac.

mod shift_support;

use serde_json::Value;
use shift_support::*;
use std::path::PathBuf;

const WG: &str = env!("CARGO_BIN_EXE_wg");

const RISKY: &str = "RISKY: add(2, 3) must be 5.";

fn submits(f: &Fx) -> usize {
    std::fs::read_to_string(f.base.join("orb-log.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("\"submit\""))
        .count()
}

fn project_test(text: &str) -> Value {
    serde_json::from_str::<Value>(text).unwrap()["commands"]["test"].clone()
}

#[test]
fn a_risky_unit_is_built_checked_and_verified_only_in_the_sandbox() {
    let Some(cli) = cli() else { return };
    let f = fx("sandbox", &cli, &["o/r"], "");
    f.with_sandbox(true);
    f.issue("o/r", 1, "Fix add with care", RISKY, "pxm-0", "pxm-0");
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    assert_eq!(out["needsYou"]["approvals"], 1, "{out}");

    let run = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    let work = f
        .claude_records(&run)
        .into_iter()
        .find(|r| r["label"] == "work:fix")
        .unwrap();
    assert_eq!(
        work["hook_sandbox_exec"], 0,
        "the guard allows wg sandbox exec for the unit"
    );
    assert_eq!(work["hook_approved_check"], 2, "and refuses the host check");
    assert_eq!(work["sandbox_exec"]["code"], 0, "{work}");
    assert_eq!(work["sandbox_exec"]["out"]["steps"][0]["exit"], 0, "{work}");
    let journal = f.run_journal(&run);
    let after: Vec<&Value> = journal
        .iter()
        .filter(|e| e["kind"] == "checks_run" && e["label"] != "check:baseline")
        .collect();
    assert!(
        !after.is_empty() && after.iter().all(|e| e["target"] == "sim_sandbox"),
        "{after:?}"
    );

    let pr = &f.prs()[0];
    let labels: Vec<&str> = pr["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"risky:auth") && labels.contains(&"wg:awaiting-approval"),
        "{labels:?}"
    );
    let body = pr["body"].as_str().unwrap();
    assert!(
        body.contains("Sandbox evidence (SandboxRun)")
            && body.contains("job j-")
            && body.contains("test exit 0"),
        "{body}"
    );
    assert!(body.contains("oreochiserver-sim"));
    assert!(f.state()["labels"]["o/r"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l == "risky:auth"));

    let shift_journal = f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(&id)
            .join("journal.jsonl"),
    );
    let fps: Vec<&Value> = shift_journal
        .iter()
        .filter(|e| e["kind"] == "argus_fingerprint")
        .collect();
    assert_eq!(fps.len(), 2, "before and after");
    assert_eq!(
        fps[0]["fingerprint"]["sha256"],
        fps[1]["fingerprint"]["sha256"]
    );
    assert!(shift_journal.iter().any(|e| e["kind"] == "sandbox_doctor"
        && e["ok"] == true
        && e["vm_host_reach"] == "accepted"));
    assert!(!shift_journal.iter().any(|e| e["kind"] == "incident"));
    let md = std::fs::read_to_string(f.home.join(".workgraph/shifts").join(&id).join("report.md"))
        .unwrap();
    assert!(
        md.contains("## Sandbox") && md.contains("vm_host_reach: accepted"),
        "{md}"
    );

    exec_returns_pending_then_poll_finishes_it(&f, &run);

    let (code, a) = f.wg(&["approve", "--repo", "o/r", "--pr", "100", "--by", "tester"]);
    assert_eq!(code, 0, "{a}");
    assert_eq!(a["passed"], true, "{a}");
    let wt = PathBuf::from(f.record("o/r", 1)["worktree"].as_str().unwrap());
    let sandbox_verify = format!("{WG} sandbox exec --task T-001 --check test");
    let evidence: Vec<Value> = std::fs::read_dir(wt.join(".haruspex/evidence/checks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
        .collect();
    assert!(
        evidence
            .iter()
            .any(|e| e["command"] == sandbox_verify.as_str() && e["status"] == "passed"),
        "verify ran in the sandbox: {evidence:?}"
    );
    // The check contract stays the Read pin: the sandbox command never reaches the PR (§9.3).
    let host = Value::from("python3 check.py");
    let on_disk = std::fs::read_to_string(wt.join(".haruspex/project.json")).unwrap();
    assert_eq!(project_test(&on_disk), host);
    let committed = sh(&wt, "git show HEAD:.haruspex/project.json");
    assert_eq!(project_test(&committed), host);
    assert_eq!(
        sh(&wt, "git status --porcelain -- .haruspex/project.json"),
        ""
    );
    assert!(
        submits(&f) >= 4,
        "worker exec, its poll test, layer and review checks, verify: {}",
        submits(&f)
    );
    assert_eq!(f.labels("o/r", 1), ["wg:verified"]);
}

/// A worker's exec that outlives its cap comes back `pending` (exit 0), and `wg sandbox poll` under
/// the same manifest finishes it from the job kept in the run (design §8.5, §15.4).
fn exec_returns_pending_then_poll_finishes_it(f: &Fx, run: &str) {
    let guard = f.home.join(".workgraph/runs").join(run).join("guard");
    let manifest = std::fs::read_dir(&guard)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.to_string_lossy().ends_with(".manifest.json")
                && serde_json::from_str::<Value>(&std::fs::read_to_string(p).unwrap()).unwrap()
                    ["sandbox"]["unit"]
                    == "fix"
        })
        .expect("the risky worker's manifest");
    std::fs::write(f.base.join("orb-pending"), "1").unwrap();
    let run_cmd = |args: &[&str]| {
        let o = f
            .command(args)
            .env("WG_GUARD_MANIFEST", &manifest)
            .env("WG_SANDBOX_EXEC_CAP_SECS", "1")
            .output()
            .unwrap();
        let text = String::from_utf8(o.stdout).unwrap();
        (
            o.status.code().unwrap(),
            serde_json::from_str::<Value>(text.trim()).unwrap(),
        )
    };
    let (code, out) = run_cmd(&[
        "sandbox", "exec", "--unit", "fix", "--", "python3", "check.py",
    ]);
    assert_eq!((code, &out["pending"]), (0, &Value::Bool(true)), "{out}");
    let job = out["job"].as_str().unwrap().to_string();
    let (code, done) = run_cmd(&["sandbox", "poll", "--job", &job]);
    assert_eq!(code, 0, "{done}");
    assert_eq!(done["steps"][0]["exit"], 0, "{done}");
    let recorded = std::fs::read_to_string(guard.parent().unwrap().join("sandbox.jsonl")).unwrap();
    assert!(recorded.lines().any(|l| l.contains(&job)), "{recorded}");
    let (code, other) = run_cmd(&["sandbox", "poll", "--job", "j-00000000"]);
    assert_eq!(code, 2);
    assert!(
        other["error"].as_str().unwrap().contains("no job"),
        "{other}"
    );
}

#[test]
fn with_the_agent_down_risky_work_parks_and_never_runs_on_the_mac() {
    let Some(cli) = cli() else { return };
    let f = fx("sandbox-down", &cli, &["o/r"], "");
    f.with_sandbox(true);
    std::fs::write(f.base.join("orb-down"), "").unwrap();
    f.issue("o/r", 1, "Fix add with care", RISKY, "pxm-0", "pxm-0");
    let out = f.shift();
    assert_eq!(out["needsYou"]["parked"], 1, "{out}");
    assert_eq!(
        f.labels("o/r", 1),
        ["wg:queued"],
        "back in the queue for when the sandbox is back"
    );
    let run = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    let journal = f.run_journal(&run);
    assert!(!journal
        .iter()
        .any(|e| e["kind"] == "llm_started"
            && e["label"].as_str().unwrap_or("").starts_with("work:")));
    assert!(
        f.comments("o/r", 1)
            .iter()
            .any(|c| c.contains("sandbox unavailable")),
        "{:?}",
        f.comments("o/r", 1)
    );
    assert!(f.prs().is_empty());
}

/// The doctor passes at shift start, then the agent dies after the first job: the task whose checks
/// meet it parks, the next risky task parks before any work, and nothing runs on the Mac. Rerun
/// once the sandbox is back, the parked task (its worktree kept, its risky unit built) is still
/// tainted, so even its baseline runs in the sandbox (design §8.1, §16 Phase 3).
#[test]
fn an_agent_that_dies_mid_shift_parks_risky_work_and_the_rerun_stays_tainted() {
    let Some(cli) = cli() else { return };
    let f = fx("sandbox-dies", &cli, &["o/r"], "");
    f.with_sandbox(true);
    std::fs::write(f.base.join("orb-down-after"), "1").unwrap();
    f.issue("o/r", 1, "Fix add with care", RISKY, "pxm-0", "pxm-0");
    f.issue("o/r", 2, "Fix add again", RISKY, "pxm-0", "pxm-0");
    let out = f.shift();
    assert_eq!(out["needsYou"]["parked"], 2, "{out}");
    assert_eq!(
        submits(&f),
        2,
        "the worker's exec, then the layer check that met the dead agent; nothing after"
    );
    for n in [1, 2] {
        assert_eq!(f.labels("o/r", n), ["wg:queued"]);
        assert!(
            f.comments("o/r", n)
                .iter()
                .any(|c| c.contains("sandbox unavailable")),
            "{:?}",
            f.comments("o/r", n)
        );
    }
    let first = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    let targets = f.check_targets(&first);
    assert_eq!(
        targets,
        [("check:baseline".to_string(), "host".to_string())],
        "no check after the baseline, none on the Mac"
    );
    let second = f.record("o/r", 2)["runs"][0].as_str().unwrap().to_string();
    assert!(!f
        .run_journal(&second)
        .iter()
        .any(|e| e["kind"] == "llm_started"
            && e["label"].as_str().unwrap_or("").starts_with("work:")));
    assert!(f.prs().is_empty());

    std::fs::remove_file(f.base.join("orb-down")).unwrap();
    std::fs::remove_file(f.base.join("orb-down-after")).unwrap();
    f.shift();
    let rerun = f.record("o/r", 1)["runs"][1].as_str().unwrap().to_string();
    let targets = f.check_targets(&rerun);
    assert_eq!(
        targets.first(),
        Some(&("check:baseline".to_string(), "sim_sandbox".to_string())),
        "the kept worktree holds code built in the sandbox: {targets:?}"
    );
    assert!(
        targets.iter().all(|(_, t)| t == "sim_sandbox"),
        "{targets:?}"
    );
}

/// A worker's exec job that leaves a container behind marks the sandbox dirty for the shift, so
/// the task's checks park instead of sending another job (design §8.5 step 6).
#[test]
fn an_exec_job_that_leaves_objects_behind_marks_the_sandbox_dirty() {
    let Some(cli) = cli() else { return };
    let f = fx("sandbox-dirty", &cli, &["o/r"], "");
    f.with_sandbox(true);
    std::fs::write(f.base.join("orb-dirty"), "").unwrap();
    f.issue("o/r", 1, "Fix add with care", RISKY, "pxm-0", "pxm-0");
    let out = f.shift();
    assert_eq!(out["needsYou"]["parked"], 1, "{out}");
    assert_eq!(submits(&f), 1, "no job after the dirty one");
    assert_eq!(f.labels("o/r", 1), ["wg:queued"]);
    let run = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    assert!(f
        .run_journal(&run)
        .iter()
        .any(|e| e["kind"] == "parked"
            && e["reason"].as_str().unwrap_or("").contains("sandbox dirty")));
    assert_eq!(f.check_targets(&run).len(), 1, "only the baseline ran");
}

/// A transient sandbox failure at approval leaves the task approvable: the failed finalize puts
/// `project.json` back, so a second `wg approve` passes (design §9.5).
#[test]
fn a_tainted_approve_that_fails_at_verify_can_be_retried() {
    let Some(cli) = cli() else { return };
    let f = fx("sandbox-retry", &cli, &["o/r"], "");
    f.with_sandbox(true);
    f.issue("o/r", 1, "Fix add with care", RISKY, "pxm-0", "pxm-0");
    f.shift();
    let wt = PathBuf::from(f.record("o/r", 1)["worktree"].as_str().unwrap());
    let approve = ["approve", "--repo", "o/r", "--pr", "100", "--by", "tester"];
    std::fs::write(f.base.join("orb-down"), "").unwrap();
    let (_, a) = f.wg(&approve);
    assert_eq!(a["passed"], false, "{a}");
    assert_eq!(a["blockedAt"], "verify", "{a}");
    let on_disk = std::fs::read_to_string(wt.join(".haruspex/project.json")).unwrap();
    assert_eq!(project_test(&on_disk), Value::from("python3 check.py"));
    std::fs::remove_file(f.base.join("orb-down")).unwrap();
    let (code, a) = f.wg(&approve);
    assert_eq!((code, &a["passed"]), (0, &Value::Bool(true)), "{a}");
    assert_eq!(f.labels("o/r", 1), ["wg:verified"]);
}
