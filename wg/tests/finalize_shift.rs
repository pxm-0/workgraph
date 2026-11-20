//! `wg approve` and `wg approval-summary` on shift PRs, in the task worktree with real Haruspex
//! (design §15.3): the head check, the check-command pin, a clean tree, refusal without tokens at
//! the current tree, refusal inside an agent, then the approve round trip to a ready PR, resumed
//! after `gh pr ready` failed once finalize had passed.

mod shift_support;

use shift_support::*;
use std::path::PathBuf;

#[test]
fn approve_makes_a_shift_pr_ready_with_haruspex_verification_recorded() {
    let Some(cli) = cli() else { return };
    let f = fx("approve", &cli, &["o/r"], "");
    f.issue(
        "o/r",
        1,
        "Fix add so it adds",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    f.issue(
        "o/r",
        2,
        "Fix add again",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    f.shift();
    let wt = PathBuf::from(f.record("o/r", 1)["worktree"].as_str().unwrap());

    let (code, s) = f.wg(&["approval-summary", "--repo", "o/r", "--pr", "100"]);
    assert_eq!(code, 0, "{s}");
    assert_eq!(
        (s["issue"].as_u64(), s["headMatches"].as_bool()),
        (Some(1), Some(true))
    );
    assert_eq!(
        (s["verified"].as_bool(), s["reviewed"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(s["checks"], serde_json::json!(["test"]));
    assert!(s["approve"]
        .as_str()
        .unwrap()
        .starts_with("wg approve --repo o/r --pr 100"));

    let approve = ["approve", "--repo", "o/r", "--pr", "100", "--by", "tester"];
    let agent = f.command(&approve).env("WG_AGENT", "1").output().unwrap();
    assert_eq!(agent.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&agent.stdout).contains("inside an agent"));

    std::fs::write(wt.join("calc.py"), "def add(a, b):\n    return b + a\n").unwrap();
    let (code, out) = f.wg(&approve);
    assert_eq!(code, 2);
    assert!(
        out["error"].as_str().unwrap().contains("tree changed"),
        "{out}"
    );
    sh(&wt, "git checkout -q -- calc.py");

    sh(&wt, "git commit -q --allow-empty -m 'local only'");
    let (code, out) = f.wg(&approve);
    assert_eq!(code, 2);
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("is not the PR head"),
        "{out}"
    );
    sh(&wt, "git reset -q --hard HEAD~1");

    // finalize passes and the evidence commit is made, then `gh pr ready` fails (every retry).
    let mut st = f.state();
    st["fail"] = serde_json::json!({ "pr ready": 3 });
    f.set_state(&st);
    let (code, out) = f.wg(&approve);
    assert_eq!(code, 2, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("fails as scripted"),
        "{out}"
    );
    assert_eq!(f.record("o/r", 1)["status"], "approved");
    assert_eq!(f.labels("o/r", 1), ["wg:pr-open"]);
    assert_eq!(f.prs()[0]["draft"], true);

    // Running it again skips finalize and finishes the push, the ready step and the label.
    let (code, out) = f.wg(&approve);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["passed"], true, "{out}");
    assert!(out["resumed"].is_string(), "{out}");
    assert_eq!(out["ready"], true);
    assert_eq!(f.labels("o/r", 1), ["wg:verified"]);
    let pr = &f.prs()[0];
    assert_eq!(pr["draft"], false, "gh pr ready ran");
    let subjects = sh(
        &f.origin("o/r"),
        "git log -2 --format=%s refs/heads/wg/1-fix-add-so-it-adds",
    );
    assert_eq!(
        subjects,
        "chore: record Haruspex verification\nfeat: Fix add so it adds"
    );
    let state = std::fs::read_to_string(wt.join(".haruspex/state.json")).unwrap();
    assert!(
        state.contains("\"tester\""),
        "the approval names the owner: {state}"
    );
    assert_eq!(f.record("o/r", 1)["status"], "verified");

    // A project.json command changed after Read: the pin refuses before any approval is recorded.
    let wt2 = PathBuf::from(f.record("o/r", 2)["worktree"].as_str().unwrap());
    let project = wt2.join(".haruspex/project.json");
    let text = std::fs::read_to_string(&project)
        .unwrap()
        .replace("python3 check.py", "python3 -c 'pass'");
    std::fs::write(&project, text).unwrap();
    let (code, out) = f.wg(&["approve", "--repo", "o/r", "--pr", "101", "--by", "tester"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("check test changed in project.json since Read"),
        "{out}"
    );
    let state2 = std::fs::read_to_string(wt2.join(".haruspex/state.json")).unwrap_or_default();
    assert!(
        !state2.contains("\"tester\""),
        "no approval recorded: {state2}"
    );
    assert_eq!(f.labels("o/r", 2), ["wg:pr-open"], "not verified");
    let (code, out) = f.wg(&["approve", "--repo", "o/r", "--pr", "999", "--by", "tester"]);
    assert_eq!(code, 2);
    assert!(
        out["error"].as_str().unwrap().contains("no shift task"),
        "{out}"
    );
}
