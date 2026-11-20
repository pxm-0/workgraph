//! A risky unit through a whole shift (design §16 Phase 3 exit, in CI form): the doctor at shift
//! start, the risky worker's only Bash being `wg sandbox exec`, every later check in the sandbox,
//! a draft PR labelled `risky:auth` carrying the SandboxRun evidence, the argus fingerprint before
//! and after, and `wg approve` verifying through the sandbox. A fake `orb` plays the agent and runs
//! the steps locally; a second shift with the agent down parks the task instead of using the Mac.

mod shift_support;

use serde_json::Value;
use shift_support::*;
use std::path::{Path, PathBuf};

const WG: &str = env!("CARGO_BIN_EXE_wg");

fn with_sandbox(f: &Fx) {
    use std::os::unix::fs::PermissionsExt;
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-orb.py");
    for name in ["orb", "orbctl"] {
        let to = f.base.join(name);
        std::fs::copy(&src, &to).unwrap();
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let text = std::fs::read_to_string(f.toml()).unwrap().replacen(
        "[tools]\n",
        &format!(
            "[tools]\norb = \"{}\"\norbctl = \"{}\"\n",
            f.base.join("orb").display(),
            f.base.join("orbctl").display()
        ),
        1,
    );
    std::fs::write(f.toml(), text).unwrap();
    f.append_toml(&format!(
        "[repo.sandbox]\nimage = \"docker.io/library/python@sha256:{}\"\nchecks = {{ test = \"python3 check.py\" }}\n\
         verify = {{ test = \"{WG} sandbox exec --task {{task}} --check test\" }}\n",
        "a".repeat(64)
    ));
}

fn records(f: &Fx, run: &str) -> Vec<Value> {
    f.journal(
        &f.home
            .join(".workgraph/runs")
            .join(run)
            .join("guard/fake-claude.jsonl"),
    )
}

#[test]
fn a_risky_unit_is_built_checked_and_verified_only_in_the_sandbox() {
    let Some(cli) = cli() else { return };
    let f = fx("sandbox", &cli, &["o/r"], "");
    with_sandbox(&f);
    f.issue(
        "o/r",
        1,
        "Fix add with care",
        "RISKY: add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    assert_eq!(out["needsYou"]["approvals"], 1, "{out}");

    let run = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    let work = records(&f, &run)
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

    let (code, a) = f.wg(&["approve", "--repo", "o/r", "--pr", "100", "--by", "tester"]);
    assert_eq!(code, 0, "{a}");
    assert_eq!(a["passed"], true, "{a}");
    let wt = PathBuf::from(f.record("o/r", 1)["worktree"].as_str().unwrap());
    let project: Value =
        serde_json::from_str(&std::fs::read_to_string(wt.join(".haruspex/project.json")).unwrap())
            .unwrap();
    assert_eq!(
        project["commands"]["test"],
        format!("{WG} sandbox exec --task T-001 --check test"),
        "verify ran in the sandbox"
    );
    let submits = std::fs::read_to_string(f.base.join("orb-log.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| l.contains("\"submit\""))
        .count();
    assert!(
        submits >= 3,
        "worker exec, layer and review checks, verify: {submits}"
    );
    assert_eq!(f.labels("o/r", 1), ["wg:verified"]);
}

#[test]
fn with_the_agent_down_risky_work_parks_and_never_runs_on_the_mac() {
    let Some(cli) = cli() else { return };
    let f = fx("sandbox-down", &cli, &["o/r"], "");
    with_sandbox(&f);
    std::fs::write(f.base.join("orb-down"), "").unwrap();
    f.issue(
        "o/r",
        1,
        "Fix add with care",
        "RISKY: add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
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
