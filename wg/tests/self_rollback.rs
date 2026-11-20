//! The rollback drill (design §12.5, §16 Phase 5 exit), automated: a good and then a bad main SHA
//! are installed from CI artifacts, the first shift on the bad binary crashes, the canary fails,
//! `current` flips back, the memory snapshot is restored, the SHA is denylisted, a revert PR opens
//! from `wg-self/revert-<sha12>` and the lane pauses.

mod self_support;

use self_support::*;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn artifact(base: &Path, name: &str, sha: &str, body: &str) -> PathBuf {
    let dir = base.join(format!("artifact-{name}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("wg"), format!("#!/bin/sh\necho {body}\n")).unwrap();
    let hash = wg::haruspex::cli::sha256_file(&dir.join("wg")).unwrap();
    fs::write(dir.join("wg.sha256"), format!("{hash}  wg\n")).unwrap();
    fs::write(dir.join("head_sha"), format!("{sha}\n")).unwrap();
    dir
}

#[test]
fn a_bad_sha_is_installed_the_canary_fails_and_everything_rolls_back() {
    let f = fx("drill", "");
    let good = sh(&f.repo, "git rev-parse HEAD");
    // The bad binary comes from a self merge on main.
    let work = f.base.join("merger");
    sh(
        &f.base,
        &format!("git clone -q {} {}", f.origin.display(), work.display()),
    );
    sh(&work, "git config user.name t && git config user.email t@example.invalid && echo bad >> docs/a.md && git commit -qam 'feat: worse prompts (#201)' && git push -q origin main");
    let bad = sh(&work, "git rev-parse HEAD");
    f.ledger()
        .append(
            "merged",
            json!({ "pr": 201, "head": "h", "merge": bad, "decision": 1 }),
        )
        .unwrap();
    let target = format!("wg-{}", wg::selfimprove::install::target());
    let (ag, ab) = (
        artifact(&f.base, "good", &good, "good"),
        artifact(&f.base, "bad", &bad, "bad"),
    );
    f.set_gh(|s| {
        s["runs"] = json!([
            { "databaseId": 11, "headSha": good, "conclusion": "success", "event": "push", "artifacts": { &target: ag } },
            { "databaseId": 12, "headSha": bad, "conclusion": "success", "event": "push", "artifacts": { &target: ab } },
        ])
    });
    let mem = f.home.join(".workgraph/memory");
    fs::create_dir_all(&mem).unwrap();
    fs::write(mem.join("o__r.json"), "{\"v\":\"good\"}").unwrap();

    let (code, v) = f.wg(&["self", "install", "--sha", &good]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = f.wg(&["self", "install", "--sha", &bad]);
    assert_eq!(code, 0, "{v}");
    let bin = f.home.join(".workgraph/bin");
    assert_eq!(
        fs::read_link(bin.join("current")).unwrap(),
        PathBuf::from(format!("wg-{}", &bad[..12]))
    );
    assert_eq!(
        fs::read_link(bin.join("previous")).unwrap(),
        PathBuf::from(format!("wg-{}", &good[..12]))
    );
    // The bad binary's runs rewrite memory; then its first shift dies before it finishes.
    fs::write(mem.join("o__r.json"), "{\"v\":\"bad\"}").unwrap();
    let hash = wg::haruspex::cli::sha256_file(&bin.join(format!("wg-{}/wg", &bad[..12]))).unwrap();
    let shift = f.home.join(".workgraph/shifts/20261121-0130");
    fs::create_dir_all(&shift).unwrap();
    fs::write(
        shift.join("journal.jsonl"),
        json!({ "seq": 1, "kind": "shift_started", "binary": &hash[..12] }).to_string() + "\n",
    )
    .unwrap();

    // GitHub is down while the revert PR opens: the rollback still completes and records it.
    f.set_gh(|s| s["fail"] = json!({ "pr create": 3 }));
    let (code, v) = f.wg(&["self", "canary", "--shift", "20261121-0130"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["healthy"], false, "{v}");
    assert!(
        v["rollback"]["revert_error"].is_string() && v["rollback"]["revert_pr"].is_null(),
        "{v}"
    );
    let kinds: Vec<String> = f
        .ledger()
        .entries()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(
        kinds.ends_with(&["rolled_back".into(), "revert_failed".into()]),
        "{kinds:?}"
    );
    // The owner's retry opens the owed revert PR from the pushed branch.
    let (code, retry) = f.wg(&["self", "rollback"]);
    assert_eq!(code, 0, "{retry}");
    assert!(
        retry["revert_pr"]
            .as_str()
            .is_some_and(|u| u.contains("/pull/")),
        "{retry}"
    );
    assert!(
        !f.home
            .join(".workgraph-work/worktrees")
            .join(format!("revert-{}", &bad[..12]))
            .exists(),
        "the revert worktree lived under worktrees/ and is gone"
    );
    assert_eq!(sh(&f.repo, "git worktree list | wc -l"), "1");
    let (code, done) = f.wg(&["self", "rollback"]);
    assert_eq!(code, 2, "nothing is owed any more: {done}");
    assert!(
        v["rollback"]["why"]
            .as_str()
            .unwrap()
            .contains("never finished"),
        "{v}"
    );
    assert_eq!(
        fs::read_link(bin.join("current")).unwrap(),
        PathBuf::from(format!("wg-{}", &good[..12]))
    );
    assert_eq!(
        fs::read_to_string(mem.join("o__r.json")).unwrap(),
        "{\"v\":\"good\"}"
    );
    let dropped = f.home.join(".workgraph/memory-dropped");
    assert_eq!(
        fs::read_dir(&dropped).unwrap().count(),
        1,
        "the bad memory is kept aside, not deleted"
    );
    let state = f.gh_state();
    let revert = state["prs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["headRefName"] == format!("wg-self/revert-{}", &bad[..12]))
        .cloned();
    let revert = revert.expect("a revert PR opened");
    assert_eq!(
        (revert["base"].as_str(), revert["draft"].as_bool()),
        (Some("main"), Some(false))
    );
    assert!(
        !revert["labels"].to_string().contains("wg:self\""),
        "never queued for auto-merge"
    );
    let msg = sh(
        &f.origin,
        &format!("git log -1 --format=%B wg-self/revert-{}", &bad[..12]),
    );
    assert!(msg.contains(&format!("This reverts commit {bad}")), "{msg}");
    let (_, st) = f.wg(&["self", "status"]);
    assert_eq!(st["paused"], true, "{st}");
    assert!(st["denylist"].to_string().contains(&bad), "{st}");
    let (code, v) = f.wg(&["self", "install", "--sha", &bad]);
    assert_eq!(code, 2);
    assert!(v["error"].as_str().unwrap().contains("denylisted"), "{v}");
    let (_, again) = f.wg(&["self", "canary", "--shift", "20261121-0130"]);
    assert_eq!(
        again["healthy"], true,
        "a shift of another binary never judges this one"
    );
}

#[test]
fn install_refuses_unverified_artifacts_and_unfinished_ci() {
    let f = fx("verify", "");
    let sha = sh(&f.repo, "git rev-parse HEAD");
    let target = format!("wg-{}", wg::selfimprove::install::target());
    let a = artifact(&f.base, "x", &sha, "x");
    let run = |dir: &Path| json!([{ "databaseId": 3, "headSha": sha, "conclusion": "success", "event": "push", "artifacts": { &target: dir } }]);
    fs::write(a.join("wg.sha256"), format!("{}  wg\n", "0".repeat(64))).unwrap();
    f.set_gh(|s| s["runs"] = run(&a));
    let (code, v) = f.wg(&["self", "install", "--sha", &sha]);
    assert!(
        code == 2 && v["error"].as_str().unwrap().contains("sha256"),
        "{v}"
    );
    let other = artifact(&f.base, "y", &"b".repeat(40), "y");
    f.set_gh(|s| s["runs"] = run(&other));
    let (_, v) = f.wg(&["self", "install", "--sha", &sha]);
    assert!(v["error"].as_str().unwrap().contains("built from"), "{v}");
    f.set_gh(|s| s["check_runs"][0]["conclusion"] = json!("failure"));
    let (_, v) = f.wg(&["self", "install", "--sha", &sha]);
    assert!(
        v["error"].as_str().unwrap().contains("required checks"),
        "{v}"
    );
    f.set_gh(|s| s["runs"] = json!([]));
    let (_, v) = f.wg(&["self", "install", "--sha", "nope"]);
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("full lowercase commit"),
        "{v}"
    );
    assert!(
        !f.home.join(".workgraph/bin/current").exists(),
        "nothing was installed"
    );
}

/// The first shift on a new binary stopped before its work (window closed, preflight): that is no
/// crash, and the binary is judged by the next shift that runs it.
#[test]
fn a_shift_that_ended_early_never_rolls_back_a_new_binary() {
    let f = fx("early", "");
    let good = sh(&f.repo, "git rev-parse HEAD");
    let target = format!("wg-{}", wg::selfimprove::install::target());
    let a = artifact(&f.base, "good", &good, "good");
    f.set_gh(|s| s["runs"] = json!([{ "databaseId": 1, "headSha": good, "conclusion": "success", "event": "push", "artifacts": { &target: a } }]));
    assert_eq!(f.wg(&["self", "install", "--sha", &good]).0, 0);
    let work = f.base.join("next");
    sh(
        &f.base,
        &format!("git clone -q {} {}", f.origin.display(), work.display()),
    );
    sh(&work, "git config user.name t && git config user.email t@example.invalid && echo n >> docs/a.md && git commit -qam next && git push -q origin main");
    let next = sh(&work, "git rev-parse HEAD");
    let b = artifact(&f.base, "next", &next, "next");
    f.set_gh(|s| s["runs"] = json!([{ "databaseId": 2, "headSha": next, "conclusion": "success", "event": "push", "artifacts": { &target: b } }]));
    assert_eq!(f.wg(&["self", "install", "--sha", &next]).0, 0);
    let bin = f.home.join(".workgraph/bin");
    let hash = wg::haruspex::cli::sha256_file(&bin.join(format!("wg-{}/wg", &next[..12]))).unwrap();
    let shift = f.home.join(".workgraph/shifts/20261121-0130");
    fs::create_dir_all(&shift).unwrap();
    let lines = [
        json!({ "seq": 1, "kind": "shift_started", "binary": &hash[..12] }),
        json!({ "seq": 2, "kind": "stopped", "why": "window closed" }),
        json!({ "seq": 3, "kind": "shift_ended_early", "why": "window closed" }),
    ];
    fs::write(
        shift.join("journal.jsonl"),
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let (code, v) = f.wg(&["self", "canary", "--shift", "20261121-0130"]);
    assert_eq!((code, v["healthy"].clone()), (0, json!(true)), "{v}");
    assert_eq!(
        fs::read_link(bin.join("current")).unwrap(),
        PathBuf::from(format!("wg-{}", &next[..12])),
        "the healthy binary stays"
    );
    assert!(
        f.ledger().of_kind("canary").unwrap().is_empty(),
        "not judged: the next shift on it is the canary"
    );
}
