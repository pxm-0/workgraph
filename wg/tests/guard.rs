//! `wg guard` as Claude Code runs it: hook JSON on stdin, a manifest path, exit 0 or 2 (design
//! §15.3). Includes issue #7: a guard built to panic still blocks.

use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct G {
    root: PathBuf,
    manifest: PathBuf,
}

fn setup(name: &str) -> G {
    let base = std::env::temp_dir().join(format!("wg-guard-it-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    for d in ["wt/src", "tmp", "home/.workgraph"] {
        fs::create_dir_all(base.join(d)).unwrap();
    }
    let root = base.canonicalize().unwrap();
    let p = |s: &str| root.join(s).to_string_lossy().into_owned();
    let cli = "/opt/haruspex/scripts/haruspex.py";
    let manifest = json!({
        "label": "read", "log": p("guard.jsonl"), "worktree": p("wt"),
        "read_roots": [p("wt"), p("tmp")], "write_roots": [p("wt/.haruspex/tasks/"), format!("{}/", p("tmp"))],
        "secret_roots": [p("home/.workgraph")],
        "tools": ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "Skill"], "skills": ["haruspex:haruspex"],
        "bash": { "exact": ["git status"], "prefixes": [], "haruspex_cli": cli,
                  "haruspex": ["init", "task create", "gate", "status"], "task_id": null, "switch_branch": true }
    });
    let path = root.join("m.json");
    fs::write(&path, manifest.to_string()).unwrap();
    G {
        root,
        manifest: path,
    }
}

fn guard(g: &G, manifest: &Path, input: &str, env: &[(&str, &str)]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args(["guard", "--manifest", manifest.to_str().unwrap()])
        .current_dir(&g.root)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

fn hook(tool: &str, input: Value) -> String {
    json!({ "tool_name": tool, "tool_input": input, "cwd": "/" }).to_string()
}

fn bash(g: &G, cmd: &str) -> i32 {
    guard(
        g,
        &g.manifest,
        &hook("Bash", json!({ "command": cmd })),
        &[],
    )
    .0
}

fn log(g: &G) -> Vec<Value> {
    fs::read_to_string(g.root.join("guard.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn denied(code: i32, stdout: &str) {
    assert_eq!(code, 2);
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn a_guard_that_panics_still_blocks() {
    let g = setup("panic");
    let (code, out) = guard(
        &g,
        &g.manifest,
        &hook("Read", json!({ "file_path": "src/a.js" })),
        &[("WG_GUARD_TEST_PANIC", "1")],
    );
    denied(code, &out);
    assert!(out.contains("panicked"));
}

#[test]
fn corrupt_or_missing_manifests_and_bad_input_deny() {
    let g = setup("corrupt");
    let bad = g.root.join("bad.json");
    fs::write(&bad, "{not json").unwrap();
    let ok_input = hook("Read", json!({ "file_path": "src/a.js" }));
    let (c, o) = guard(&g, &bad, &ok_input, &[]);
    denied(c, &o);
    let (c, o) = guard(&g, &g.root.join("missing.json"), &ok_input, &[]);
    denied(c, &o);
    let (c, o) = guard(&g, &g.manifest, "not json", &[]);
    denied(c, &o);
    let (c, o) = guard(&g, &g.manifest, &hook("Teleport", json!({})), &[]);
    denied(c, &o);
    let usage = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args(["guard"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
}

#[test]
fn reads_in_the_worktree_and_run_tmp_are_allowed_and_secrets_are_incidents() {
    let g = setup("reads");
    let read = |p: String| {
        guard(
            &g,
            &g.manifest,
            &hook("Read", json!({ "file_path": p })),
            &[],
        )
        .0
    };
    assert_eq!(read("src/a.js".into()), 0);
    assert_eq!(read(g.root.join("tmp/notes.txt").display().to_string()), 0);
    assert_eq!(read("/etc/hosts".into()), 2);
    assert_eq!(
        read(g.root.join("home/.workgraph/key").display().to_string()),
        2
    );
    let decisions: Vec<String> = log(&g)
        .iter()
        .map(|e| e["decision"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(decisions, ["allow", "allow", "deny", "incident"]);
}

#[test]
fn haruspex_grammar_blocks_are_not_incidents_and_approve_is() {
    let g = setup("grammar");
    let hx = |a: &str| format!("python3 /opt/haruspex/scripts/haruspex.py {a}");
    assert_eq!(bash(&g, &hx("init .")), 0);
    assert_eq!(bash(&g, &hx("gate ready-to-build . --json")), 0);
    assert_eq!(bash(&g, &hx("init /other --force")), 2);
    assert_eq!(bash(&g, &hx("task create --force T .")), 2);
    assert_eq!(bash(&g, &hx("gate ready-to-build . --record")), 2);
    let blocks = log(&g);
    assert!(
        blocks[2..].iter().all(|e| e["decision"] == "deny"),
        "{blocks:?}"
    );
    assert_eq!(bash(&g, &hx("approve ready-to-build . --by me")), 2);
    assert_eq!(log(&g).last().unwrap()["decision"], "incident");
}

#[test]
fn the_permission_layer_escapes_are_refused_with_a_positive_control() {
    // ADR-001 §13 replayed against the real guard: the plain command runs, every metacharacter
    // form is denied.
    let g = setup("meta");
    assert_eq!(bash(&g, "git status"), 0, "positive control");
    for cmd in [
        "git status; touch x",
        "git status && touch x",
        "git status | tee x",
        "git status $(touch x)",
        "git status `touch x`",
        "git status > x",
        "git status\ntouch x",
        "git switch -c work/ok; touch x",
    ] {
        assert_eq!(bash(&g, cmd), 2, "{cmd:?}");
    }
    assert_eq!(bash(&g, "git switch -c work/ok"), 0);
    let edit = |p: &str| {
        guard(
            &g,
            &g.manifest,
            &hook("Write", json!({ "file_path": p, "content": "" })),
            &[],
        )
        .0
    };
    assert_eq!(edit(".haruspex/tasks/T-001.json"), 0);
    assert_eq!(edit(".haruspex/state.json"), 2);
    assert_eq!(log(&g).last().unwrap()["decision"], "incident");
}
