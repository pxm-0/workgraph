// Integration tests: run the real wg binary in throwaway git repos.
// The finalize tests need the Haruspex CLI: HARUSPEX_CLI=/abs/haruspex.py cargo test
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wg-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    sh(&dir, "git init -q -b main && git config user.name t && git config user.email t@example.invalid && echo a > a.txt && git add -A && git commit -qm init");
    dir
}

fn sh(dir: &Path, script: &str) {
    let ok = Command::new("sh").arg("-c").arg(script).current_dir(dir).status().unwrap().success();
    assert!(ok, "setup failed: {script}");
}

fn wg(dir: &Path, args: &[&str], stdin: Option<&str>) -> (i32, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.unwrap_or("").as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "wg must print exactly one line: {text}");
    (out.status.code().unwrap(), serde_json::from_str(&text).unwrap())
}

const MEMORY: &str = r#"{"version":1,"runs":2,"nextId":2,"checks":["npm test"],"rules":[{"id":"r1","text":"x","scope":"worker","sig":"s","a":2,"b":1}],"models":{}}"#;

#[test]
fn recall_reports_memory_and_dirty_paths() {
    let d = repo("recall");
    let (code, v) = wg(&d, &["recall"], None);
    assert_eq!(code, 0);
    assert_eq!(v["exists"], false);
    assert_eq!(v["dirty"], serde_json::json!([]));

    sh(&d, "mkdir -p .claude && echo '{}' > .claude/work-memory.json && echo b > b.txt && git mv a.txt moved.txt");
    let (_, v) = wg(&d, &["recall"], None);
    assert_eq!(v["exists"], true);
    assert_eq!(v["content"], "{}\n");
    let dirty: Vec<&str> = v["dirty"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
    assert!(dirty.contains(&"b.txt") && dirty.contains(&"moved.txt") && dirty.contains(&".claude/work-memory.json"), "{dirty:?}");
    assert!(!dirty.contains(&"a.txt"), "a rename reports its new path only");
}

#[test]
fn check_echoes_the_nonce_and_reports_exit_codes_and_tails() {
    let d = repo("check");
    let (code, v) = wg(&d, &["check", "--nonce", "run1:L1:1", "--cmd", "echo ok", "--cmd", "for i in $(seq 1 60); do echo line$i; done; echo boom >&2; exit 3"], None);
    assert_eq!(code, 0);
    assert_eq!(v["nonce"], "run1:L1:1");
    assert_eq!(v["results"][0]["exitCode"], 0);
    assert_eq!(v["results"][0]["outputTail"], "ok");
    let failing = &v["results"][1];
    assert_eq!(failing["exitCode"], 3);
    let tail = failing["outputTail"].as_str().unwrap();
    assert_eq!(tail.lines().count(), 40, "only the last 40 lines are kept");
    assert!(tail.ends_with("boom"), "stderr is interleaved with stdout");
    assert_eq!(v["changedFiles"], serde_json::json!([]));
}

#[test]
fn check_kills_a_command_that_runs_past_its_timeout() {
    let d = repo("timeout");
    let start = Instant::now();
    let (_, v) = wg(&d, &["check", "--nonce", "n", "--timeout-secs", "1", "--cmd", "sleep 30 & sleep 30; wait"], None);
    assert!(start.elapsed().as_secs() < 10, "the whole process group is killed");
    assert_eq!(v["results"][0]["exitCode"], 124);
    assert!(v["results"][0]["outputTail"].as_str().unwrap().contains("timed out after 1s"));
}

#[test]
fn save_writes_validated_memory_with_a_backup() {
    let d = repo("save");
    let (code, v) = wg(&d, &["save"], Some(MEMORY));
    assert_eq!((code, v["saved"].clone(), v["rules"].clone()), (0, true.into(), 1.into()));
    let (code, _) = wg(&d, &["save"], Some(MEMORY));
    assert_eq!(code, 0);
    assert!(d.join(".claude/work-memory.json.bak").exists(), "the previous file is backed up");
    let saved: Value = serde_json::from_str(&fs::read_to_string(d.join(".claude/work-memory.json")).unwrap()).unwrap();
    assert_eq!(saved["rules"][0]["id"], "r1");
}

#[test]
fn save_refuses_bad_input_and_never_overwrites_a_corrupt_file() {
    let d = repo("save-bad");
    let bad_rule = MEMORY.replace("\"scope\":\"worker\"", "\"scope\":\"nobody\"");
    let (code, v) = wg(&d, &["save"], Some(&bad_rule));
    assert_eq!(code, 2);
    assert!(v["error"].as_str().unwrap().contains("rules"));

    sh(&d, "mkdir -p .claude && printf '{broken' > .claude/work-memory.json");
    let (code, v) = wg(&d, &["save"], Some(MEMORY));
    assert_eq!(code, 2);
    assert!(v["error"].as_str().unwrap().contains("left untouched"));
    assert_eq!(fs::read_to_string(d.join(".claude/work-memory.json")).unwrap(), "{broken");
}

#[test]
fn finalize_commits_records_gates_and_verifies_with_real_haruspex() {
    let Ok(cli) = std::env::var("HARUSPEX_CLI") else {
        eprintln!("skipped: set HARUSPEX_CLI=/abs/path/haruspex.py to run the finalize tests");
        return;
    };
    let d = repo("finalize");
    // A repo with a filled-in Haruspex task, a built change and a passed review, as /work leaves it.
    sh(&d, &format!(r#"
        printf '{{"scripts":{{"test":"true"}}}}\n' > package.json && git add -A && git commit -qm pkg
        python3 '{cli}' init . --project-name t >/dev/null && python3 '{cli}' task create "Greeting" . --id T-001 >/dev/null
        p=.haruspex/project.json; jq '.project.risk="low" | .project.owner="t" | .commands.unit_test="npm test" | .characteristics |= with_entries(.value=false)' $p > x && mv x $p
        t=.haruspex/tasks/T-001.json; jq '.status="ready" | .risk="low"
          | .problem={{summary:"s",affected_actors:["dev"],current_behavior:"c",desired_outcome:"d",success_metrics:["m"]}}
          | .scope={{included:["a.txt"],excluded:["x"]}} | .test_plan.unit=["u"] | .required_checks=["unit_test"]
          | .review_coverage.core |= with_entries(.value={{status:"complete",rationale:"r"}})
          | .acceptance_criteria=[{{id:"AC-001",criterion:"a.txt says hello",status:"passed",evidence:["check:unit_test"]}}]
          | .independent_review={{status:"passed",reviewer:"workgraph reviewer (opus)",at:"2026-11-20T00:00:00Z",findings:[],waiver_reason:null}}' $t > x && mv x $t
        echo hello > a.txt && echo mine > wip.txt
    "#));
    let (code, v) = wg(&d, &["finalize", "--task", "T-001", "--by", "tester", "--cli", &cli, "--message", "feat: hello", "--evidence-message", "chore: evidence", "--exclude-json", r#"["wip.txt"]"#], None);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["passed"], true, "{v:#}");
    assert!(!v["staged"].as_array().unwrap().contains(&"wip.txt".into()), "the user's own work is never staged");
    let log = String::from_utf8(Command::new("git").args(["log", "--format=%s"]).current_dir(&d).output().unwrap().stdout).unwrap();
    assert!(log.starts_with("chore: evidence\nfeat: hello\n"), "{log}");
    let status = String::from_utf8(Command::new("git").args(["status", "--porcelain"]).current_dir(&d).output().unwrap().stdout).unwrap();
    assert_eq!(status.trim(), "?? wip.txt");
}

#[test]
fn finalize_stops_at_the_first_blocked_gate() {
    let Ok(cli) = std::env::var("HARUSPEX_CLI") else { return };
    let d = repo("finalize-blocked");
    sh(&d, &format!("python3 '{cli}' init . >/dev/null && python3 '{cli}' task create t . --id T-001 >/dev/null && echo x > a.txt"));
    let (code, v) = wg(&d, &["finalize", "--task", "T-001", "--by", "tester", "--cli", &cli, "--message", "feat: x", "--evidence-message", "chore: e"], None);
    assert_eq!(code, 0);
    assert_eq!(v["passed"], false);
    assert_eq!(v["blockedAt"], "gate ready-to-build", "an empty Reading cannot pass the build gate: {v:#}");
    assert!(v["steps"].as_array().unwrap().iter().all(|s| s["step"] != "verify"), "nothing runs after a blocked gate");
}
