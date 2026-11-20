//! `wg eval capture` (design §12.4, §15.4): a merged PR becomes a fixture pushed as a
//! `fixture/<n>-<slug>` branch to a local bare eval origin, with a draft PR for the owner; the eval
//! repo's main is never pushed, and an existing fixture branch is never forced over.

mod self_support;

use self_support::*;
use serde_json::json;
use std::fs;

#[test]
fn a_merged_pr_becomes_a_fixture_branch_and_main_is_never_pushed() {
    let eval = std::env::temp_dir().join(format!("wg-eval-origin-{}", std::process::id()));
    let _ = fs::remove_dir_all(&eval);
    fs::create_dir_all(&eval).unwrap();
    sh(&eval, "git init -q --bare -b main");
    let f = fx("capture", &format!("eval_remote = \"{}\"", eval.display()));
    let base = sh(&f.repo, "git rev-parse HEAD");
    let work = f.base.join("feature");
    sh(
        &f.base,
        &format!("git clone -q {} {}", f.origin.display(), work.display()),
    );
    sh(
        &work,
        "git config user.name t && git config user.email t@example.invalid \
               && echo 'pub fn sub() {}' >> wg-factory/src/plan/mod.rs \
               && printf '#[test]\\nfn sub_works() {}\\n' > wg-factory/tests/sub_test.rs \
               && git add -A && git commit -qm 'feat: add sub (#7)' && git push -q origin main",
    );
    let merge = sh(&work, "git rev-parse HEAD");
    f.set_gh(|s| {
        s["origins"]["pxm-0/workgraph-eval"] = json!(eval);
        s["issues"][REPO] = json!({ "7": { "number": 7, "title": "Add sub()", "body": "sub(5, 3) is 2.",
            "author": { "login": "pxm-0" }, "labels": [], "url": "https://github.com/pxm-0/workgraph/issues/7",
            "createdAt": "2026-11-20T00:00:00Z" } });
        s["prs"] = json!([{ "repo": REPO, "number": 31, "url": "https://github.com/pxm-0/workgraph/pull/31",
            "state": "MERGED", "headRefName": "wg/7-add-sub", "headRefOid": merge, "mergeCommit": merge, "base": "main",
            "title": "feat: add sub", "body": "Closes #7\n\nDraft PR by a workgraph shift.", "draft": false, "labels": [] }]);
    });
    let url = "https://github.com/pxm-0/workgraph/pull/31";
    let (code, v) = f.wg(&["eval", "capture", "--pr", url, "--expect", "done"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["branch"], "fixture/31-feat-add-sub", "{v}");
    assert_eq!(v["base"], base.as_str());
    assert_eq!(v["hidden"], json!(["wg-factory/tests/sub_test.rs"]));
    let heads = sh(&eval, "git for-each-ref --format='%(refname)' refs/heads");
    assert_eq!(
        heads, "refs/heads/fixture/31-feat-add-sub",
        "only the fixture branch, never main"
    );
    let store = f.base.join("store-check");
    sh(
        &f.base,
        &format!(
            "git clone -q -b fixture/31-feat-add-sub {} {}",
            eval.display(),
            store.display()
        ),
    );
    let fx_dir = store.join("fixtures/31-feat-add-sub");
    assert!(fs::read_to_string(fx_dir.join("task.md"))
        .unwrap()
        .contains("sub(5, 3) is 2."));
    assert!(fx_dir.join("hidden/wg-factory/tests/sub_test.rs").is_file());
    sh(&fx_dir, "git bundle verify -q repo.bundle");
    let loaded = wg::selfimprove::eval_live::fixtures(&store).unwrap();
    assert_eq!(
        (loaded[0].repo.as_str(), loaded[0].base_commit.as_str()),
        (REPO, base.as_str())
    );
    let draft = f.gh_state()["prs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["repo"] == "pxm-0/workgraph-eval")
        .cloned()
        .unwrap();
    assert_eq!(
        (draft["draft"].as_bool(), draft["base"].as_str()),
        (Some(true), Some("main"))
    );
    // A second capture never forces over the branch it already pushed (an identical one is a no-op).
    f.set_gh(|s| s["issues"][REPO]["7"]["body"] = json!("sub(5, 3) is 2, and sub(0, 1) is -1."));
    let (code, v) = f.wg(&["eval", "capture", "--pr", url]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains("push"), "{v}");
    let (code, v) = f.wg(&["eval", "capture", "--pr", url, "--expect", "maybe"]);
    assert!(
        code == 2 && v["error"].as_str().unwrap().contains("--expect"),
        "{v}"
    );
    let _ = fs::remove_dir_all(&eval);
}

#[test]
fn replay_runs_from_the_cli_against_a_base_checkouts_cases() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = std::process::Command::new(WG)
        .args(["eval", "replay", "--cases"])
        .arg(root.join("eval/replay"))
        .arg("--manifest")
        .arg(root.join("eval/manifest.toml"))
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        (out.status.code(), v["failed"].clone()),
        (Some(0), json!(0)),
        "{v}"
    );
    let moved = std::env::temp_dir().join(format!("wg-replay-moved-{}", std::process::id()));
    let _ = fs::remove_dir_all(&moved);
    fs::create_dir_all(&moved).unwrap();
    let out = std::process::Command::new(WG)
        .args(["eval", "replay", "--cases"])
        .arg(&moved)
        .arg("--manifest")
        .arg(root.join("eval/manifest.toml"))
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        v["error"].as_str().unwrap().contains("the manifest pins 3"),
        "{v}"
    );
}

#[test]
fn a_history_holding_an_env_file_is_never_captured() {
    let eval = std::env::temp_dir().join(format!("wg-eval-env-{}", std::process::id()));
    let _ = fs::remove_dir_all(&eval);
    fs::create_dir_all(&eval).unwrap();
    sh(&eval, "git init -q --bare -b main");
    let f = fx(
        "captureenv",
        &format!("eval_remote = \"{}\"", eval.display()),
    );
    let work = f.base.join("feature");
    sh(
        &f.base,
        &format!("git clone -q {} {}", f.origin.display(), work.display()),
    );
    // A secret committed once and deleted later is still in every bundle of that history.
    sh(
        &work,
        "git config user.name t && git config user.email t@example.invalid \
               && echo 'TOKEN=x' > .env.local && echo 'TOKEN=' > .env.example && git add -A && git commit -qm oops \
               && git rm -q .env.local && git commit -qm 'remove it' \
               && echo 'pub fn sub() {}' >> wg-factory/src/plan/mod.rs \
               && git commit -qam 'feat: add sub (#8)' && git push -q origin main",
    );
    let merge = sh(&work, "git rev-parse HEAD");
    f.set_gh(|s| {
        s["origins"]["pxm-0/workgraph-eval"] = json!(eval);
        s["prs"] = json!([{ "repo": REPO, "number": 32, "url": "https://github.com/pxm-0/workgraph/pull/32",
            "state": "MERGED", "headRefName": "wg/8-add-sub", "headRefOid": merge, "mergeCommit": merge, "base": "main",
            "title": "feat: add sub", "body": "Add sub.", "draft": false, "labels": [] }]);
    });
    let (code, v) = f.wg(&[
        "eval",
        "capture",
        "--pr",
        "https://github.com/pxm-0/workgraph/pull/32",
    ]);
    assert_eq!(code, 2, "{v}");
    assert!(v["error"].as_str().unwrap().contains(".env.local"), "{v}");
    assert_eq!(
        sh(&eval, "git for-each-ref refs/heads | wc -l"),
        "0",
        "nothing was pushed"
    );
    let _ = fs::remove_dir_all(&eval);
}
