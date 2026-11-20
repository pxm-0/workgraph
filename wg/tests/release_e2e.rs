//! Phase 4 end to end (design §16 Phase 4 exit, in CI form): one shift takes a queued issue through
//! dev and QA to a draft PR; the owner approves it (`wg approve`); while the PR is open the release
//! line waits; once it merges, the next shift recreates a worktree at the merge commit, verifies
//! there with the pinned commands, writes notes and a plan, rehearses in the sandbox (a fake `orb`)
//! and stops at the ready-to-release human stage: nothing is approved, recorded or deployed.

mod shift_support;

use serde_json::{json, Value};
use shift_support::*;
use std::path::PathBuf;

fn shift_journal(f: &Fx, id: &str) -> Vec<Value> {
    f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(id)
            .join("journal.jsonl"),
    )
}

fn kinds(j: &[Value], kind: &str) -> Vec<Value> {
    j.iter().filter(|e| e["kind"] == kind).cloned().collect()
}

/// Merges the task branch into main on the origin, as the owner would, and tells the fake gh.
fn merge(f: &Fx, branch: &str) -> String {
    let clone = f.base.join("owner-merge");
    sh(
        &f.base,
        &format!(
            "git clone -q {} {} && cd {} && git config user.name owner && git config user.email o@example.invalid \
             && git fetch -q origin {branch} && git merge -q --no-ff -m 'Merge {branch}' FETCH_HEAD && git push -q origin main",
            f.origin("o/r").display(),
            clone.display(),
            clone.display()
        ),
    );
    let sha = sh(&clone, "git rev-parse HEAD");
    let mut st = f.state();
    st["prs"][0]["state"] = json!("MERGED");
    st["prs"][0]["mergeCommit"] = json!(sha);
    f.set_state(&st);
    sha
}

#[test]
fn dev_qa_draft_pr_approval_then_release_stops_at_the_owners_gate() {
    let Some(cli) = cli() else { return };
    let f = fx("release", &cli, &["o/r"], "");
    f.with_sandbox(false);
    f.append_toml("release_rehearsal = [\"python3 check.py\"]");
    f.append_toml(
        "[[check]]\nrepo = \"o/r\"\nname = \"e2e\"\nproject = \"python3 check.py\"\nfactory = \"python3 check.py\"",
    );
    f.set_repo("o/r", "qa_checks", "[\"e2e\"]");
    f.set_repo("o/r", "release", "true");
    f.issue(
        "o/r",
        1,
        "Fix add so it adds",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );

    // Shift 1: dev → qa → draft PR, in one run.
    let out = f.shift();
    assert_eq!(out["needsYou"]["approvals"], 1, "{out}");
    let run = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    let journal = f.run_journal(&run);
    let checks: Vec<String> = kinds(&journal, "checks_run")
        .iter()
        .map(|e| e["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(checks, ["check:baseline", "check:L1", "check:regression"]);
    let qa = kinds(&journal, "checks_run")[2]["results"].clone();
    assert_eq!(
        qa,
        json!([{ "name": "test", "exit": 0 }, { "name": "e2e", "exit": 0 }])
    );
    let calls: Vec<String> = f
        .claude_records(&run)
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        calls,
        ["read", "challenge", "work:fix", "review#1", "explore"]
    );
    assert_eq!(
        journal.iter().find(|e| e["kind"] == "run_started").unwrap()["pipeline"],
        "dev+qa"
    );
    let pr = &f.prs()[0];
    assert_eq!(
        (pr["draft"].as_bool(), pr["number"].as_u64()),
        (Some(true), Some(100))
    );
    assert_eq!(f.labels("o/r", 1), ["wg:pr-open"]);

    // The owner approves: Haruspex verification recorded, the PR ready, the issue verified.
    let (code, a) = f.wg(&["approve", "--repo", "o/r", "--pr", "100", "--by", "tester"]);
    assert_eq!((code, a["passed"].as_bool()), (0, Some(true)), "{a}");
    assert_eq!(f.labels("o/r", 1), ["wg:verified"]);

    // Shift 2: the PR is still open, so the release line waits.
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    let waits = kinds(&shift_journal(&f, &id), "release_waits");
    assert_eq!(waits[0]["state"], "OPEN", "{waits:?}");
    assert_eq!(f.record("o/r", 1)["status"], "verified");

    // The owner merges; gc removes the task worktree but keeps the task verified for the release
    // line, and shift 3 prepares the release at the merge commit.
    let sha = merge(&f, "wg/1-fix-add-so-it-adds");
    let (code, gc) = f.wg(&["shift", "gc"]);
    assert_eq!(
        (code, gc["removed"].as_array().map(Vec::len)),
        (0, Some(1)),
        "{gc}"
    );
    assert_eq!(f.record("o/r", 1)["status"], "verified");
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    assert_eq!(out["needsYou"]["releases"], 1, "{out}");
    let sj = shift_journal(&f, &id);
    let fin = &kinds(&sj, "release_finished")[0];
    assert_eq!(
        (fin["outcome"].as_str(), fin["merge"].as_str()),
        (Some("release-ready"), Some(sha.as_str())),
        "{sj:?}"
    );
    let rel = fin["run"].as_str().unwrap();
    let rj = f.run_journal(rel);
    assert_eq!(
        rj.iter().find(|e| e["kind"] == "run_started").unwrap()["pipeline"],
        "release"
    );
    let rcalls: Vec<String> = f
        .claude_records(rel)
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(rcalls, ["notes", "plan"]);
    let rehearsal = &kinds(&rj, "sandbox_run")[0];
    assert_eq!(rehearsal["steps"][0]["name"], "release-rehearsal-1");
    assert_eq!(rehearsal["steps"][0]["exit"], 0);
    let gate = &kinds(&rj, "gate_evaluated")[0];
    assert_eq!(
        gate["blockers"],
        json!(["Human approval for ready_to_release is pending."]),
        "{gate}"
    );
    assert!(rj
        .iter()
        .any(|e| e["kind"] == "parked" && e["human"] == "ready-to-release"));

    // At the merge commit, verified there, the plan written, and nothing approved or deployed.
    let wt = PathBuf::from(rel_worktree(&f));
    assert_eq!(sh(&wt, "git rev-parse HEAD"), sha);
    let task: Value = serde_json::from_str(
        &std::fs::read_to_string(wt.join(".haruspex/tasks/T-001.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(task["status"], "ready_for_release");
    assert!(task["release_plan"]["rollback_steps"][0].is_string());
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(wt.join(".haruspex/state.json")).unwrap())
            .unwrap();
    assert_eq!(state["approvals"]["ready_to_release"]["status"], "pending");
    assert!(state["gates"]["ready_to_release"]["status"] != "passed");
    let evidence = sh(&wt, "ls .haruspex/evidence/checks | wc -l");
    assert!(
        evidence.parse::<u32>().unwrap() >= 2,
        "verify ran again here"
    );

    assert_eq!(f.labels("o/r", 1), ["wg:release-ready"]);
    assert_eq!(f.record("o/r", 1)["status"], "release-ready");
    let comment = f.comments("o/r", 1).last().unwrap().clone();
    assert!(
        comment.contains(&format!("merge commit {sha}"))
            && comment.contains("never deploys")
            && comment.contains("add now adds"),
        "{comment}"
    );
    let log = f.state()["log"].to_string();
    assert!(!log.contains("\"merge\"") && !log.contains("\"release\",\"create\""));
    // The owner's checkout was never touched.
    assert_eq!(sh(&f.repo("o/r"), "git status --porcelain"), "");

    // A prepared release is not prepared again.
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    assert!(kinds(&shift_journal(&f, &id), "release_started").is_empty());
}

fn rel_worktree(f: &Fx) -> String {
    f.home
        .join(".workgraph-work/worktrees/o__r/release-1")
        .to_string_lossy()
        .into_owned()
}
