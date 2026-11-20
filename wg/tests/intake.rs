//! Intake, claim, recover and gc against a fake gh (design §15.3): label-actor filtering, a body
//! edited by an untrusted actor after labelling, factory-created and `wg:self-proposed` issues,
//! ordering, the claim, and stale claims requeued.

use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use wg::gh::Gh;
use wg::shift::{intake, record};
use wg::state_dirs::Dirs;

fn base(name: &str) -> PathBuf {
    let b = std::env::temp_dir().join(format!("wg-intake-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&b);
    fs::create_dir_all(b.join("home/.workgraph")).unwrap();
    let gh = b.join("fake-gh.py");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.py"),
        &gh,
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    b
}

fn issue(n: u64, author: &str, labeller: &str, labels: &[&str], edits: Value) -> Value {
    json!({ "number": n, "title": format!("Issue {n}"), "body": "do it", "author": { "login": author },
            "labels": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
            "url": format!("https://github.com/o/r/issues/{n}"), "createdAt": format!("2026-11-01T10:00:{n:02}Z"),
            "events": [{ "label": "wg:queued", "actor": labeller, "at": "2026-11-02T00:00:00Z" }], "edits": edits })
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn only_trusted_issues_are_taken_p1_first_then_oldest_and_the_claim_relabels() {
    let b = base("filter");
    let state = b.join("state.json");
    let q = ["wg:queued"];
    let after = json!([{ "at": "2026-11-03T00:00:00Z", "editor": "stranger" }]);
    let before = json!([{ "at": "2026-11-01T12:00:00Z", "editor": "stranger" }]);
    let mut issues = json!({
        "1": issue(1, "pxm-0", "pxm-0", &q, json!([])),
        "2": issue(2, "pxm-0", "stranger", &q, json!([])),
        "3": issue(3, "stranger", "pxm-0", &q, json!([])),
        "4": issue(4, "pxm-0", "pxm-0", &q, after),
        "5": issue(5, "pxm-0", "pxm-0", &q, before),
        "6": issue(6, "pxm-0", "pxm-0", &["wg:queued", "wg:self-proposed"], json!([])),
        "7": issue(7, "pxm-0", "pxm-0", &q, json!([])),
        "8": issue(8, "pxm-0", "pxm-0", &["wg:queued", "wg:p1"], json!([])),
        "9": issue(9, "pxm-0", "pxm-0", &["wg:queued", "wg:pr-open"], json!([])),
    });
    issues["10"] = issue(10, "pxm-0", "pxm-0", &q, json!([]));
    // Parked, edited by mallory while claimed, then queued again by the factory's own token.
    let requeued = |n: u64| {
        let mut i = issue(
            n,
            "pxm-0",
            "pxm-0",
            &q,
            json!([{ "at": "2026-11-02T06:00:00Z", "editor": "mallory" }]),
        );
        i["events"] = json!([{ "label": "wg:queued", "actor": "pxm-0", "at": "2026-11-02T00:00:00Z" },
                             { "label": "wg:queued", "actor": "pxm-0", "at": "2026-11-02T08:00:00Z" }]);
        i
    };
    issues["11"] = requeued(11);
    issues["12"] = requeued(12);
    issues["13"] = issue(13, "pxm-0", "pxm-0", &q, json!([]));
    issues["13"]["events"] = json!([{ "label": "wg:queued", "actor": "pxm-0", "at": "2026-11-02T00:00:00Z" },
        { "event": "renamed", "actor": "mallory", "at": "2026-11-02T09:00:00Z" }]);
    issues["10"]["body"] = json!("x".repeat(20_001));
    fs::write(
        &state,
        json!({ "issues": { "o/r": issues }, "prs": [], "origins": {} }).to_string(),
    )
    .unwrap();
    // One test owns this process's FAKE_GH_STATE, so nothing else races on it.
    std::env::set_var("FAKE_GH_STATE", &state);
    let dirs = Dirs {
        home: b.join("home"),
    };
    intake::ledger(
        &dirs,
        json!({ "kind": "created", "repo": "o/r", "issue": 7 }),
    )
    .unwrap();
    let gh = Gh {
        delays: vec![],
        ..Gh::new(b.join("fake-gh.py").to_str().unwrap(), b.join("scratch"))
    };
    let cfg = wg::config::parse("[intake]\ntrusted_actors = [\"pxm-0\"]\n").unwrap();
    let got = intake::intake(
        &gh,
        &cfg.shift,
        "o/r",
        &intake::created(&dirs, "o/r"),
        &[11],
    )
    .unwrap();
    let picked: Vec<u64> = got.picked.iter().map(|i| i.number).collect();
    assert_eq!(picked, [8, 1, 5, 12]);
    let why = |n: u64| {
        got.skipped
            .iter()
            .find(|(x, _)| *x == n)
            .map(|(_, w)| w.clone())
            .unwrap_or_default()
    };
    assert!(why(2).contains("labelled wg:queued by stranger"));
    assert!(why(3).contains("written by stranger"));
    assert!(why(4).contains("edited by stranger after labelling"));
    assert!(why(6).contains("wg:self-proposed"));
    assert!(why(7).contains("created by the factory"));
    assert!(why(9).contains("wg:pr-open"));
    assert!(why(0).contains("body over 20000"), "{:?}", got.skipped);
    assert!(
        why(11).contains("edited by mallory"),
        "a rerun is checked back to the owner's first label: {:?}",
        got.skipped
    );
    assert!(why(13).contains("edited by mallory"), "an untrusted rename");

    // Just before a claim the issue is read again: closed, un-queued or edited means no claim.
    let still = |n: u64| {
        intake::still_queued(
            &gh,
            &cfg.shift,
            "o/r",
            &got.picked.iter().find(|i| i.number == n).unwrap().clone(),
        )
    };
    assert_eq!(still(1), None);
    let mut st = read(&state);
    st["issues"]["o/r"]["5"]["state"] = json!("CLOSED");
    st["issues"]["o/r"]["8"]["labels"] = json!([{ "name": "wg:queued" }, { "name": "wg:stuck" }]);
    st["issues"]["o/r"]["12"]["body"] = json!("do something else");
    fs::write(&state, st.to_string()).unwrap();
    assert!(still(5).unwrap().contains("closed"));
    assert!(still(8).unwrap().contains("wg:stuck"));
    assert!(still(12).unwrap().contains("changed after intake"));

    intake::claim(&dirs, &gh, &cfg.shift, "o/r", 1, "20261120-0130").unwrap();
    let s = read(&state);
    let i1 = &s["issues"]["o/r"]["1"];
    assert_eq!(i1["labels"], json!([{ "name": "wg:in-shift" }]));
    assert!(i1["comments"][0]
        .as_str()
        .unwrap()
        .contains("20261120-0130"));
    assert!(intake::relabel(&dirs, &gh, "o/r", 1, &["wg:self"], &[])
        .unwrap_err()
        .contains("owner"));
    let ledger = fs::read_to_string(dirs.intake_ledger()).unwrap();
    assert!(ledger
        .lines()
        .any(|l| l.contains("\"kind\":\"label\"") && l.contains("wg:in-shift")));
}

fn wg(home: &Path, state: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args(args)
        .env("HOME", home)
        .env("FAKE_GH_STATE", state)
        .env_remove("WG_AGENT")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
}

#[test]
fn recover_requeues_stale_claims_without_a_pr_and_gc_removes_merged_worktrees() {
    let b = base("recover");
    let home = b.join("home");
    let dirs = Dirs { home: home.clone() };
    let repo = b.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let ok = |dir: &Path, s: &str| {
        assert!(
            Command::new("sh")
                .arg("-c")
                .arg(s)
                .current_dir(dir)
                .status()
                .unwrap()
                .success(),
            "{s}"
        )
    };
    ok(&repo, "git init -q -b main && git config user.name t && git config user.email t@x.invalid && git commit -q --allow-empty -m init && git update-ref refs/remotes/origin/main HEAD");
    fs::write(
        home.join(".workgraph/factory.toml"),
        format!(
            "[tools]\ngh = \"{}\"\n[[repo]]\nname = \"o/r\"\npath = \"{}\"\n",
            b.join("fake-gh.py").display(),
            repo.display()
        ),
    )
    .unwrap();
    let state = b.join("state.json");
    let q = ["wg:in-shift"];
    fs::write(&state, json!({ "origins": { "o/r": repo.join(".git") }, "issues": { "o/r": {
        "1": issue(1, "pxm-0", "pxm-0", &q, json!([])), "2": issue(2, "pxm-0", "pxm-0", &q, json!([])),
        "3": issue(3, "pxm-0", "pxm-0", &q, json!([])),
        "4": issue(4, "pxm-0", "pxm-0", &["wg:in-shift", "wg:stuck"], json!([])) } },
        "prs": [{ "repo": "o/r", "number": 100, "url": "https://github.com/o/r/pull/100", "state": "MERGED",
                  "headRefName": "wg/2-two" }] }).to_string()).unwrap();
    let wt = wg::git::worktree(&repo, &dirs.worktrees(), "o__r/2-two", "wg/2-two", "main").unwrap();
    let rec =
        |n: u64, branch: &str, updated: u64, pr: Option<u64>, worktree: &Path| record::TaskRecord {
            repo: "o/r".into(),
            issue: n,
            branch: branch.into(),
            status: "in-shift".into(),
            updated_at: updated,
            pr,
            worktree: worktree.to_string_lossy().into_owned(),
            ..Default::default()
        };
    let old = wg::journal::now() - 25 * 3600;
    record::save(
        &dirs,
        &rec(1, "wg/1-one", old, None, Path::new("/nonexistent")),
    )
    .unwrap();
    record::save(&dirs, &rec(2, "wg/2-two", old, Some(100), &wt)).unwrap();
    record::save(
        &dirs,
        &rec(4, "wg/4-four", old, None, Path::new("/nonexistent")),
    )
    .unwrap();
    record::save(
        &dirs,
        &rec(
            3,
            "wg/3-three",
            wg::journal::now(),
            None,
            Path::new("/nonexistent"),
        ),
    )
    .unwrap();

    let out = wg(&home, &state, &["shift", "recover"]);
    assert_eq!(
        out["recovered"],
        json!([{ "repo": "o/r", "issue": 1 }]),
        "#2 has a PR, #3 is fresh, #4 already carries wg:stuck"
    );
    assert_eq!(
        read(&state)["issues"]["o/r"]["4"]["labels"],
        json!([{ "name": "wg:in-shift" }, { "name": "wg:stuck" }])
    );
    let s = read(&state);
    assert_eq!(
        s["issues"]["o/r"]["1"]["labels"],
        json!([{ "name": "wg:queued" }])
    );
    assert_eq!(
        s["issues"]["o/r"]["3"]["labels"],
        json!([{ "name": "wg:in-shift" }])
    );

    let out = wg(&home, &state, &["shift", "gc"]);
    assert_eq!(out["removed"][0]["issue"], 2, "{out}");
    assert!(!wt.exists());
    assert_eq!(
        record::load(&dirs, "o/r", 2).unwrap().unwrap().status,
        "gone"
    );
    let st = wg(&home, &state, &["shift", "status"]);
    assert_eq!(st["tasksByStatus"]["gone"], 1);
    assert_eq!(st["stop"], false);
}
