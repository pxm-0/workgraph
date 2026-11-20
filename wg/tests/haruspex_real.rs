//! Against the real Haruspex CLI (design §15.3): the pinned-path refusals, the golden blocker
//! string, the approval invariant on a repo with a prior approved task, and the contract snapshot
//! of a real task file. Needs HARUSPEX_CLI=/abs/haruspex.py.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use wg::config::Pin;
use wg::haruspex::cli::{sha256_file, PinnedHaruspex};
use wg::haruspex::{contract, tamper};
use wg::ports::Haruspex;

fn repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wg-hxreal-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let ok = Command::new("sh")
        .arg("-c")
        .arg("git init -q -b main && git config user.name t && git config user.email t@example.invalid && echo a > a.txt && git add -A && git commit -qm init")
        .current_dir(&dir)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    dir.canonicalize().unwrap()
}

fn hx_cmd(dir: &Path, cli: &str, args: &[&str]) {
    let out = Command::new("python3")
        .arg(cli)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn pinned(cli: &str, dir: &Path) -> PinnedHaruspex {
    let pin = Pin {
        cli: cli.into(),
        sha256: sha256_file(Path::new(cli)).unwrap(),
    };
    PinnedHaruspex::new(&pin, dir, &[]).unwrap()
}

#[test]
fn the_pin_refuses_a_cli_inside_the_repo_or_with_another_hash() {
    let Ok(cli) = std::env::var("HARUSPEX_CLI") else {
        return;
    };
    let dir = repo("pin");
    let inside = dir.join("haruspex.py");
    fs::copy(&cli, &inside).unwrap();
    let pin = Pin {
        cli: inside.to_string_lossy().into(),
        sha256: sha256_file(&inside).unwrap(),
    };
    let err = PinnedHaruspex::new(&pin, &dir, &[]).err().unwrap();
    assert!(err.contains("lies inside"), "{err}");
    let wrong = Pin {
        cli: cli.clone(),
        sha256: "0".repeat(64),
    };
    assert!(PinnedHaruspex::new(&wrong, &dir, &[])
        .err()
        .unwrap()
        .contains("incident"));
}

#[test]
fn the_golden_blocker_string_and_an_honest_read_pass_the_approval_invariant() {
    let Ok(cli) = std::env::var("HARUSPEX_CLI") else {
        return;
    };
    let dir = repo("invariant");
    hx_cmd(&dir, &cli, &["init", "."]);
    hx_cmd(&dir, &cli, &["task", "create", "First", ".", "--id", "T-1"]);
    let h = pinned(&cli, &dir);
    let blockers = h.gate_ready_to_build().unwrap();
    assert!(
        blockers.contains(&wg::engine::kernel::PENDING_APPROVAL.to_string()),
        "Haruspex changed its pending-approval blocker: {blockers:?}"
    );
    hx_cmd(
        &dir,
        &cli,
        &["approve", "ready-to-build", ".", "--by", "owner"],
    );
    let before = h.approvals().unwrap();
    assert_eq!(
        before["ready_to_build"]["status"], "approved",
        "a prior approved task"
    );
    hx_cmd(
        &dir,
        &cli,
        &["task", "create", "Second", ".", "--id", "T-2"],
    );
    let after_read = h.approvals().unwrap();
    assert_eq!(
        tamper::violation(&before, &after_read),
        None,
        "task create resets approvals: an honest Read passes"
    );
    hx_cmd(
        &dir,
        &cli,
        &["approve", "ready-to-build", ".", "--by", "agent"],
    );
    let planted = h.approvals().unwrap();
    assert!(
        tamper::violation(&after_read, &planted).is_some(),
        "a planted approval fails"
    );
}

#[test]
fn the_contract_snapshot_reads_a_real_task_file() {
    let Ok(cli) = std::env::var("HARUSPEX_CLI") else {
        return;
    };
    let dir = repo("contract");
    hx_cmd(&dir, &cli, &["init", "."]);
    hx_cmd(&dir, &cli, &["task", "create", "Real", ".", "--id", "T-9"]);
    let h = pinned(&cli, &dir);
    let mut task: Value = h.task("T-9").unwrap();
    task["required_checks"] = serde_json::json!(["test"]);
    task["failure_modes"] = serde_json::json!([{ "id": "FM-001", "scenario": "s", "severity": "critical", "status": "open" }]);
    let before = contract::snapshot(&task);
    assert!(
        before.criteria.contains("AC-001"),
        "the template's first criterion"
    );
    task["failure_modes"][0]["status"] = serde_json::json!("accepted_risk");
    let after = contract::snapshot(&task);
    assert!(contract::weakened(&before, &after, &[])
        .unwrap()
        .contains("FM-001"));
    assert!(
        h.managed_hash().unwrap() != "absent",
        "init installs the managed tree"
    );
    assert!(h.task("../escape").is_err());
}
