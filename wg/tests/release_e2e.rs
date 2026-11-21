//! Phase 4 end to end (design §16 Phase 4 exit, in CI form): one shift takes a queued issue through
//! dev and QA to a draft PR; the owner approves it (`wg approve`); while the PR is open the release
//! line waits; once it merges, the next shift recreates a worktree at the merge commit, verifies
//! there with the pinned commands, writes notes and a plan, rehearses in the sandbox (a fake `orb`)
//! and stops at the ready-to-release human stage: nothing is approved, recorded or deployed. Also:
//! a seeded e2e failure fixed by one blamed round, re-reviewed and published as a draft PR; the
//! release line after the dev limits; a release incident; and a PR that merged past the verified
//! head.

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

/// Pushes one more commit to the task branch on the origin, as anyone with write access could.
fn push_to_branch(f: &Fx, branch: &str) {
    let clone = f.base.join("late-push");
    sh(
        &f.base,
        &format!(
            "git clone -q {} {} && cd {} && git config user.name late && git config user.email l@example.invalid \
             && git checkout -q {branch} && echo late > late.txt && git add late.txt && git commit -qm late \
             && git push -q origin {branch}",
            f.origin("o/r").display(),
            clone.display(),
            clone.display()
        ),
    );
}

/// A release-on repo whose issue 1 went through dev and QA to a PR the owner approved and merged
/// (after `late`, a commit pushed past the approval).
fn approved_and_merged(name: &str, cli: &str, late: bool, rehearsal: &str) -> Fx {
    let f = fx(name, cli, &["o/r"], "");
    f.with_sandbox(false);
    f.append_toml(&format!("release_rehearsal = [\"{rehearsal}\"]"));
    f.set_repo("o/r", "release", "true");
    f.issue(
        "o/r",
        1,
        "Fix add so it adds",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    assert_eq!(f.shift()["needsYou"]["approvals"], 1);
    let (code, a) = f.wg(&["approve", "--repo", "o/r", "--pr", "100", "--by", "tester"]);
    assert_eq!((code, a["passed"].as_bool()), (0, Some(true)), "{a}");
    if late {
        push_to_branch(&f, "wg/1-fix-add-so-it-adds");
    }
    merge(&f, "wg/1-fix-add-so-it-adds");
    f
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
    // `o/s` comes first and later holds more queued issues than max_tasks.
    let f = fx("release", &cli, &["o/s", "o/r"], "max_tasks = 1");
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
    // The backlog outlasts max_tasks: intake ends after one task, the release line still runs.
    for n in [1, 2] {
        f.issue(
            "o/s",
            n,
            "Fix add so it adds",
            "add(2, 3) must be 5.",
            "pxm-0",
            "pxm-0",
        );
    }
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    assert_eq!(out["needsYou"]["releases"], 1, "{out}");
    let sj = shift_journal(&f, &id);
    assert_eq!(kinds(&sj, "task_finished").len(), 1, "{sj:?}");
    assert!(kinds(&sj, "stopped")[0]["why"]
        .as_str()
        .unwrap()
        .contains("max_tasks"));
    assert_eq!(f.labels("o/s", 2), ["wg:queued"]);
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

    // The release worktree is scratch once the chain stops at the owner's gate.
    let wt = PathBuf::from(rel_worktree(&f));
    assert!(!wt.exists(), "{}", wt.display());
    assert!(!sh(&f.repo("o/r"), "git worktree list").contains("release-1"));
    assert_eq!(kinds(&sj, "release_worktree_removed")[0]["issue"], 1);

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

#[test]
fn a_seeded_e2e_failure_is_fixed_by_one_blamed_round_re_reviewed_and_published() {
    let Some(cli) = cli() else { return };
    let f = fx("qa-e2e", &cli, &["o/r"], "");
    std::fs::write(
        f.repo("o/r").join("e2e.py"),
        "import calc\nassert getattr(calc, 'E2E', False), 'e2e: the session survives logout'\n",
    )
    .unwrap();
    f.push("o/r", "git add e2e.py");
    f.append_toml(
        "[[check]]\nrepo = \"o/r\"\nname = \"e2e\"\nproject = \"python3 e2e.py\"\nfactory = \"python3 e2e.py\"",
    );
    f.set_repo("o/r", "qa_checks", "[\"e2e\"]");
    f.issue(
        "o/r",
        1,
        "Fix add so it adds",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    let out = f.shift();
    assert_eq!(out["needsYou"]["approvals"], 1, "{out}");
    let run = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    let runs = kinds(&f.run_journal(&run), "checks_run");
    let checks: Vec<(String, Value)> = runs
        .iter()
        .map(|e| {
            (
                e["label"].as_str().unwrap().to_string(),
                e["results"].clone(),
            )
        })
        .collect();
    assert_eq!(
        checks[2..],
        [
            (
                "check:regression".to_string(),
                json!([{ "name": "test", "exit": 0 }, { "name": "e2e", "exit": 1 }])
            ),
            (
                "check:regression#1".to_string(),
                json!([{ "name": "test", "exit": 0 }, { "name": "e2e", "exit": 0 }])
            ),
        ]
    );
    let calls: Vec<String> = f
        .claude_records(&run)
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        calls,
        [
            "read",
            "challenge",
            "work:fix",
            "review#1",
            "regression:fix#1",
            "explore",
            "rereview#1"
        ]
    );
    let pr = &f.prs()[0];
    assert_eq!(pr["draft"].as_bool(), Some(true), "{pr}");
    assert_eq!(f.labels("o/r", 1), ["wg:pr-open"]);
}

#[test]
fn a_release_incident_labels_the_issue_with_its_class_only_and_stops_the_shift() {
    let Some(cli) = cli() else { return };
    let f = approved_and_merged("release-incident", &cli, false, "python3 check.py");
    std::fs::write(f.home.join("release-incident"), "").unwrap();
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    let sj = shift_journal(&f, &id);
    let incident = &kinds(&sj, "incident")[0];
    assert_eq!(incident["at"], "release", "{sj:?}");
    assert!(kinds(&sj, "stopped")
        .iter()
        .any(|e| e["why"] == "an incident in the release line"));
    assert_eq!(kinds(&sj, "release_finished")[0]["outcome"], "incident");
    assert!(out["needsYou"]["incidents"].as_u64().unwrap() >= 1, "{out}");
    assert_eq!(f.labels("o/r", 1), ["wg:incident"]);
    assert_eq!(f.record("o/r", 1)["status"], "incident");
    let comment = f.comments("o/r", 1).last().unwrap().clone();
    assert!(
        comment.contains("(class: approval)") && !comment.contains("outside finalize"),
        "{comment}"
    );
}

#[test]
fn a_pr_merged_past_the_verified_head_is_release_stuck_listed_and_retryable() {
    let Some(cli) = cli() else { return };
    let f = approved_and_merged("release-late", &cli, true, "python3 check.py");
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    let sj = shift_journal(&f, &id);
    assert!(kinds(&sj, "release_started").is_empty(), "nothing ran");
    let fin = &kinds(&sj, "release_finished")[0];
    assert_eq!(fin["outcome"], "release-stuck");
    assert!(
        fin["note"].as_str().unwrap().contains("never reviewed"),
        "{fin}"
    );
    assert_eq!(f.record("o/r", 1)["status"], "release-stuck");
    assert_eq!(f.labels("o/r", 1), ["wg:verified"]);
    let md = std::fs::read_to_string(out["report"].as_str().unwrap()).unwrap();
    assert!(
        md.contains("### Releases stuck\n- o/r#1 (PR #100): refused: the PR merged at head")
            && md.contains("`wg shift retry-release --repo o/r --issue 1`"),
        "{md}"
    );
    let (code, r) = f.wg(&["shift", "retry-release", "--repo", "o/r", "--issue", "1"]);
    assert_eq!((code, &r["status"]), (0, &json!("verified")), "{r}");
    assert_eq!(f.record("o/r", 1)["status"], "verified");
}

#[test]
fn a_release_stuck_after_its_chain_ran_keeps_its_worktree() {
    let Some(cli) = cli() else { return };
    let f = approved_and_merged("release-stuck-kept", &cli, false, "exit 1");
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    let sj = shift_journal(&f, &id);
    assert_eq!(kinds(&sj, "release_started").len(), 1, "{sj:?}");
    assert_eq!(
        kinds(&sj, "release_finished")[0]["outcome"],
        "release-stuck"
    );
    assert!(kinds(&sj, "release_worktree_removed").is_empty());
    assert!(PathBuf::from(rel_worktree(&f)).exists());
}
