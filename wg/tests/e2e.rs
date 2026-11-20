//! End to end through the binary: `wg run --experimental` in a throwaway git repo with a scripted
//! `claude` (tests/fixtures/fake-claude.py) and the real, pinned Haruspex CLI, then
//! `wg finalize --run`. Needs HARUSPEX_CLI=/abs/haruspex.py, as cli.rs does.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Toy {
    home: PathBuf,
    repo: PathBuf,
}

fn sh(dir: &Path, script: &str) {
    let ok = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "setup failed: {script}");
}

fn wg(t: &Toy, args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args(args)
        .current_dir(&t.repo)
        .env("HOME", &t.home)
        .env_remove("WG_AGENT")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.lines().count(),
        1,
        "one JSON line: {text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        out.status.code().unwrap(),
        serde_json::from_str(&text).unwrap(),
    )
}

fn toy(name: &str, cli: &str) -> Toy {
    let base = std::env::temp_dir().join(format!("wg-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (home, repo) = (base.join("home"), base.join("repo"));
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(home.join(".workgraph")).unwrap();
    sh(
        &repo,
        "git init -q -b main && git config user.name t && git config user.email t@example.invalid",
    );
    fs::write(repo.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    fs::write(
        repo.join("check.py"),
        "import glob, runpy\nfor f in sorted(glob.glob('test_*.py')):\n    runpy.run_path(f)\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::write(
        repo.join(".claude/work-memory.json"),
        r#"{"version":1,"runs":3,"nextId":2,"checks":["python3 check.py"],"rules":[{"id":"r1","text":"Keep calc pure","scope":"worker","sig":"s","a":2,"b":1}],"models":{}}"#,
    )
    .unwrap();
    sh(&repo, "git add -A && git commit -qm init");
    let repo = repo.canonicalize().unwrap();
    let fake = base.join("fake-claude.py");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.py"),
        &fake,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let cli = Path::new(cli).canonicalize().unwrap();
    let sha = wg::haruspex::cli::sha256_file(&cli).unwrap();
    let name = wg::git::repo_name(&wg::git::slug(&repo));
    fs::write(
        home.join(".workgraph/factory.toml"),
        format!(
            "[tools]\nclaude = \"{}\"\n\n[haruspex]\ncli = \"{}\"\nsha256 = \"{sha}\"\n\n[[check]]\nrepo = \"{name}\"\nname = \"test\"\nproject = \"python3 check.py\"\nfactory = \"python3 check.py\"\n",
            fake.display(),
            cli.display()
        ),
    )
    .unwrap();
    fs::write(base.join("task.txt"), "Fix add so it adds.").unwrap();
    Toy { home, repo }
}

fn run(t: &Toy) -> Value {
    let task = t.repo.parent().unwrap().join("task.txt");
    let (code, out) = wg(
        t,
        &[
            "run",
            "--experimental",
            "--task-file",
            task.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{out}");
    out
}

fn lines(p: &Path) -> Vec<Value> {
    fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_run_reaches_ready_for_review_and_finalize_run_passes_the_verification_gate() {
    let Ok(cli) = std::env::var("HARUSPEX_CLI") else {
        eprintln!("skipped: set HARUSPEX_CLI=/abs/path/haruspex.py to run the e2e test");
        return;
    };
    let t = toy("pass", &cli);
    let out = run(&t);
    assert_eq!(out["status"], "ready_for_review", "{out}");
    assert_eq!(
        (out["verified"].as_bool(), out["reviewed"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(out["done"], serde_json::json!(["fix"]));
    assert_eq!(out["memorySaved"], true);
    let run_dir = PathBuf::from(out["logDir"].as_str().unwrap());

    // The guard hook from the per-call settings blocks what the role may not do.
    let fake = lines(&run_dir.join("guard/fake-claude.jsonl"));
    let work = fake.iter().find(|r| r["label"] == "work:fix").unwrap();
    assert_eq!(work["hook_edit_unit_file"], 0);
    assert_eq!(work["hook_edit_other_file"], 2);
    assert_eq!(work["hook_chained_bash"], 2);
    assert_eq!(work["hook_approved_check"], 0);
    for r in &fake {
        let env: Vec<&str> = r["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            env.iter()
                .all(|k| !k.contains("KEY") && !k.contains("TOKEN")),
            "{env:?}"
        );
        assert!(env.contains(&"WG_AGENT"));
    }
    let journal = lines(&run_dir.join("journal.jsonl"));
    assert!(
        !journal.iter().any(|e| e["kind"] == "incident"),
        "zero incidents"
    );
    assert!(!lines(&run_dir.join("guard.jsonl"))
        .iter()
        .any(|e| e["decision"] == "incident"));
    let ledger = lines(&t.home.join(".workgraph/cost.jsonl"));
    let labels: Vec<&str> = ledger
        .iter()
        .map(|l| l["label"].as_str().unwrap())
        .collect();
    // learn runs too: check.py leaves __pycache__/ outside the plan, which is learn evidence.
    assert_eq!(
        labels,
        ["read", "challenge", "work:fix", "review#1", "learn"]
    );
    assert!(ledger
        .iter()
        .all(|l| l["provider"] == "subscription" && l["model_id"].is_string()));

    // Memory moved out of the repo: imported once, then saved by Rust as valid v1.
    let slug = wg::git::slug(&t.repo);
    let mem: Value = serde_json::from_str(
        &fs::read_to_string(t.home.join(format!(".workgraph/memory/{slug}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(wg::memory::validate(&mem), Ok(()));
    assert_eq!(mem["runs"], 4, "the imported 3 runs plus this one");
    assert_eq!(mem["rules"][0]["id"], "r1");

    let run_id = out["runId"].as_str().unwrap();
    let (code, fin) = wg(
        &t,
        &[
            "finalize",
            "--run",
            run_id,
            "--by",
            "tester",
            "--message",
            "fix: add adds",
            "--evidence-message",
            "chore: record Haruspex verification",
        ],
    );
    assert_eq!(code, 0, "{fin}");
    assert_eq!(fin["passed"], true, "{fin}");
    let log = Command::new("git")
        .args(["log", "--format=%s"])
        .current_dir(&t.repo)
        .output()
        .unwrap();
    let subjects = String::from_utf8(log.stdout).unwrap();
    assert!(
        subjects.starts_with("chore: record Haruspex verification\nfix: add adds\n"),
        "{subjects}"
    );
}

#[test]
fn finalize_run_refuses_when_the_tree_moved_or_inside_an_agent() {
    let Ok(cli) = std::env::var("HARUSPEX_CLI") else {
        return;
    };
    let t = toy("moved", &cli);
    let out = run(&t);
    let run_id = out["runId"].as_str().unwrap().to_string();
    let agent = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "finalize",
            "--run",
            &run_id,
            "--by",
            "x",
            "--message",
            "m",
            "--evidence-message",
            "e",
        ])
        .current_dir(&t.repo)
        .env("HOME", &t.home)
        .env("WG_AGENT", "1")
        .output()
        .unwrap();
    assert_eq!(agent.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&agent.stdout).contains("inside an agent"));
    fs::write(
        t.repo.join("calc.py"),
        "def add(a, b):\n    return b + a  # edited after the run\n",
    )
    .unwrap();
    let (code, fin) = wg(
        &t,
        &[
            "finalize",
            "--run",
            &run_id,
            "--by",
            "tester",
            "--message",
            "m",
            "--evidence-message",
            "e",
        ],
    );
    assert_eq!(code, 2);
    assert!(
        fin["error"].as_str().unwrap().contains("tree changed"),
        "{fin}"
    );
    let (code, fin) = wg(
        &t,
        &[
            "finalize",
            "--run",
            "nope-123456",
            "--by",
            "t",
            "--message",
            "m",
            "--evidence-message",
            "e",
        ],
    );
    assert_eq!(code, 2);
    assert!(fin["error"].as_str().unwrap().contains("no finished run"));
}
