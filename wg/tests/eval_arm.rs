//! `wg eval arm` (design §12.4), the real Tier B arm, with the fake claude, the fake orb sandbox
//! and the real Haruspex CLI: a throwaway clone of the fixture's bundle at its base commit, the
//! dev chain with base assets or the candidate's overlay, every check in the sandbox, a throwaway
//! memory, and the hidden tests copied in only after the run, then run in the sandbox.

mod shift_support;

use serde_json::Value;
use shift_support::*;
use std::fs;
use std::path::Path;

/// A one-fixture store: the toy repo `o/r` bundled at its head, a task, and a hidden test that
/// passes only when add adds.
fn store(f: &Fx) -> std::path::PathBuf {
    let dir = f.base.join("store/fixtures/f1");
    fs::create_dir_all(dir.join("hidden")).unwrap();
    let repo = f.repo("o/r");
    let base = sh(&repo, "git rev-parse HEAD");
    sh(
        &repo,
        &format!(
            "git bundle create -q {} HEAD main",
            dir.join("repo.bundle").display()
        ),
    );
    fs::write(dir.join("task.md"), "Fix add\n\nadd(2, 3) must be 5.\n").unwrap();
    fs::write(
        dir.join("hidden/test_hidden.py"),
        "from calc import add\nassert add(2, 3) == 5\n",
    )
    .unwrap();
    fs::write(
        dir.join("fixture.toml"),
        format!(
            "repo = \"o/r\"\nbase_commit = \"{base}\"\nexpected_status = \"done\"\nimage = \"docker.io/library/python@sha256:{}\"\nchecks = {{ hidden = \"python3 check.py\" }}\n",
            "b".repeat(64)
        ),
    )
    .unwrap();
    dir
}

fn arm(f: &Fx, fixture: &Path, extra: &[&str]) -> Value {
    let fx = fixture.to_string_lossy();
    let mut args = vec!["eval", "arm", "--fixture", &fx, "--tag", "eval-7"];
    args.extend(extra);
    let (code, v) = f.wg(&args);
    assert_eq!(code, 0, "{v}");
    v
}

fn assets_hash(f: &Fx, run: &str) -> String {
    f.run_journal(run)
        .iter()
        .find(|e| e["kind"] == "run_started")
        .unwrap()["assets"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn each_arm_runs_the_fixture_in_the_sandbox_and_sees_hidden_tests_only_after() {
    let Some(cli) = cli() else { return };
    let f = fx("evalarm", &cli, &["o/r"], "");
    f.with_sandbox(false);
    let fixture = store(&f);
    let base = arm(&f, &fixture, &["--arm", "base"]);
    assert_eq!(
        (
            base["status"].as_str(),
            base["passed"].as_bool(),
            base["schema_ok"].as_bool()
        ),
        (Some("ready_for_review"), Some(true), Some(true)),
        "{base}"
    );
    assert_eq!(base["criteria"], serde_json::json!(["AC-001"]), "{base}");
    assert_eq!(base["units"], 1, "{base}");
    assert!(
        base["gates"].as_array().is_some_and(|g| !g.is_empty()),
        "{base}"
    );
    assert!(
        base["cost_micros"].is_u64() && base["by_provider"].is_object(),
        "{base}"
    );
    let run = base["run"].as_str().unwrap();
    let work = f
        .claude_records(run)
        .into_iter()
        .find(|r| r["label"].as_str().is_some_and(|l| l.starts_with("work:")))
        .unwrap();
    let seen = work["files_seen"].to_string();
    assert!(
        seen.contains("calc.py") && !seen.contains("test_hidden.py"),
        "hidden tests are never readable during the run: {seen}"
    );
    let targets = f.check_targets(run);
    assert!(
        !targets.is_empty() && targets.iter().all(|(_, t)| t == "sim_sandbox"),
        "every check of an arm runs in the sandbox: {targets:?}"
    );
    let memory = f.home.join(".workgraph/memory");
    assert!(
        !memory.exists() || fs::read_dir(&memory).unwrap().count() == 0,
        "an eval never learns into the owner's memory"
    );

    // The candidate arm runs the same fixture with the overlay's prompts.
    let overlay = f.base.join("overlay");
    fs::create_dir_all(overlay.join("prompts")).unwrap();
    let work_prompt =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../prompts/work.md"))
            .unwrap();
    fs::write(
        overlay.join("prompts/work.md"),
        format!("{work_prompt}\nBe brief.\n"),
    )
    .unwrap();
    let ov = overlay.to_string_lossy().to_string();
    let cand = arm(&f, &fixture, &["--arm", "candidate", "--overlay", &ov]);
    assert_eq!(cand["passed"], true, "{cand}");
    let crun = cand["run"].as_str().unwrap();
    assert_ne!(
        assets_hash(&f, run),
        assets_hash(&f, crun),
        "the candidate ran its own prompts"
    );

    // A hidden test the work does not satisfy fails the arm's pass signal.
    fs::write(
        fixture.join("hidden/test_hidden.py"),
        "from calc import add\nassert add(2, 3) == 6\n",
    )
    .unwrap();
    let failing = arm(&f, &fixture, &["--arm", "base"]);
    assert_eq!(
        (failing["status"].as_str(), failing["passed"].as_bool()),
        (Some("ready_for_review"), Some(false)),
        "{failing}"
    );
}
