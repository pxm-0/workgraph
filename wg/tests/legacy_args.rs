//! A bad argument to a legacy subcommand prints one JSON error line and exits 2 (design §15.3);
//! hyphen-leading flag values are accepted as values.
use serde_json::{json, Value};
use std::process::{Command, Stdio};

fn wg(args: &[&str]) -> (i32, Value) {
    let dir = std::env::temp_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args(args)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.lines().count(),
        1,
        "wg must print exactly one line: {text}"
    );
    (
        out.status.code().unwrap(),
        serde_json::from_str(&text).unwrap(),
    )
}

fn repo(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wg-args-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init failed");
    dir
}

#[test]
fn bad_arguments_print_one_json_error_line_and_exit_2() {
    let cli = "/abs/haruspex.py";
    let finalize = [
        "finalize",
        "--task",
        "T",
        "--by",
        "b",
        "--message",
        "m",
        "--evidence-message",
        "e",
    ];
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![], "usage: wg recall | check | save | finalize"),
        (vec!["nope"], "usage: wg recall | check | save | finalize"),
        (vec!["check", "stray"], "unexpected argument: stray"),
        (vec!["check", "--nonce"], "--nonce needs a value"),
        (vec!["check", "--cmd", "true"], "--nonce is required"),
        (
            vec!["check", "--nonce", "n", "--timeout-secs", "soon"],
            "--timeout-secs must be a whole number",
        ),
        (finalize[..7].to_vec(), "--cli is required"),
        (
            [&finalize[..], &["--cli", "rel/haruspex.py"]].concat(),
            "--cli must be an absolute path",
        ),
        (
            [&finalize[..], &["--cli", cli, "--exclude-json", "{"]].concat(),
            "--exclude-json must be a JSON array of paths",
        ),
    ];
    for (args, expected) in cases {
        let (code, v) = wg(&args);
        assert_eq!(code, 2, "{args:?}: {v}");
        let error = v["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{args:?}: no error field in {v}"));
        assert!(error.starts_with(expected), "{args:?}: {error}");
    }
}

#[test]
fn hyphen_leading_values_are_values_not_flags() {
    let d = repo("hyphen");
    let out = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args(["check", "--nonce", "-n1", "--cmd", "-v; echo after"])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["nonce"], "-n1");
    assert_eq!(v["results"][0]["cmd"], "-v; echo after");
    assert_eq!(v["results"][0]["exitCode"], 0);
    assert!(v["results"][0]["outputTail"]
        .as_str()
        .unwrap()
        .ends_with("after"));
    assert_eq!(v["changedFiles"], json!([]));
}

#[test]
fn finalize_with_nothing_to_commit_is_an_error() {
    let d = repo("finalize-empty");
    let out = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "finalize",
            "--task",
            "T",
            "--by",
            "b",
            "--cli",
            "/abs/h.py",
            "--message",
            "m",
            "--evidence-message",
            "e",
        ])
        .current_dir(&d)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        json!({ "error": "nothing to commit outside the excluded paths" })
    );
}

#[test]
fn recall_outside_a_git_repo_is_an_error() {
    let dir = std::env::temp_dir().join(format!("wg-args-nogit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_wg"))
        .arg("recall")
        .current_dir(&dir)
        .env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("git status failed:"),
        "{v}"
    );
}
