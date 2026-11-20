//! The owner's commands through the binary: config allow-check and pin-haruspex, memory import,
//! doctor, and `wg auth key` with a fake keychain holding a sentinel key.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SENTINEL: &str = "sk-ant-api03-SENTINEL-not-a-real-key";

fn home(name: &str) -> PathBuf {
    let h = std::env::temp_dir().join(format!("wg-admin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&h);
    fs::create_dir_all(&h).unwrap();
    h.canonicalize().unwrap()
}

fn wg_raw(home: &Path, cwd: &Path, args: &[&str], agent: bool) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_wg"));
    c.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("WG_AGENT")
        .env_remove("ANTHROPIC_API_KEY");
    if agent {
        c.env("WG_AGENT", "1");
    }
    c.output().unwrap()
}

fn wg(home: &Path, cwd: &Path, args: &[&str]) -> (i32, Value) {
    let out = wg_raw(home, cwd, args, false);
    (
        out.status.code().unwrap(),
        serde_json::from_slice(&out.stdout).unwrap(),
    )
}

#[test]
fn allow_check_appends_entries_that_load_and_override() {
    let h = home("allow");
    let args = |cmd: &'static str| {
        [
            "config",
            "allow-check",
            "--repo",
            "o/r",
            "--name",
            "test",
            "--cmd",
            cmd,
            "--factory-cmd",
            cmd,
        ]
    };
    let (code, v) = wg(&h, &h, &args("npm test"));
    assert_eq!((code, v["allowed"].as_bool()), (0, Some(true)));
    let (code, _) = wg(&h, &h, &args("npm run test:ci"));
    assert_eq!(code, 0);
    let cfg = wg::config::load(&h.join(".workgraph/factory.toml")).unwrap();
    assert_eq!(
        cfg.check("o/r", "test").unwrap().project,
        "npm run test:ci",
        "the later entry wins"
    );
    let (code, v) = wg(
        &h,
        &h,
        &[
            "config",
            "allow-check",
            "--repo",
            "o/r",
            "--name",
            "Bad Name",
            "--cmd",
            "x",
            "--factory-cmd",
            "x",
        ],
    );
    assert_eq!(code, 2);
    assert!(v["error"].as_str().unwrap().contains("check name"));
    assert_eq!(
        wg::config::load(&h.join(".workgraph/factory.toml"))
            .unwrap()
            .checks
            .len(),
        2,
        "a bad entry is never written"
    );
    let agent = wg_raw(&h, &h, &args("x"), true);
    assert_eq!(agent.status.code(), Some(2), "owner-only");
    let (code, _) = wg(&h, &h, &["config", "nope"]);
    assert_eq!(code, 2);
}

#[test]
fn pin_haruspex_hashes_an_outside_cli_and_refuses_one_inside_the_repo() {
    let h = home("pin");
    let tools = h.join("tools");
    fs::create_dir_all(&tools).unwrap();
    let cli = tools.join("haruspex.py");
    fs::write(&cli, "print('stub')\n").unwrap();
    let (code, v) = wg(
        &h,
        &h,
        &["config", "pin-haruspex", "--cli", cli.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{v}");
    let cfg = wg::config::load(&h.join(".workgraph/factory.toml")).unwrap();
    assert_eq!(
        cfg.haruspex.unwrap().sha256,
        wg::haruspex::cli::sha256_file(&cli).unwrap()
    );
    let repo = h.join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    let inside = repo.join("haruspex.py");
    fs::write(&inside, "x").unwrap();
    let (code, v) = wg(
        &h,
        &repo,
        &["config", "pin-haruspex", "--cli", inside.to_str().unwrap()],
    );
    assert_eq!(code, 2);
    assert!(v["error"].as_str().unwrap().contains("lies inside"));
}

#[test]
fn memory_import_copies_legacy_memory_once() {
    let h = home("import");
    let repo = h.join("repo");
    fs::create_dir_all(repo.join(".claude")).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    fs::write(
        repo.join(".claude/work-memory.json"),
        r#"{"version":1,"runs":7,"nextId":1,"checks":[],"rules":[],"models":{}}"#,
    )
    .unwrap();
    let (code, v) = wg(&h, &repo, &["memory", "import"]);
    assert_eq!((code, v["imported"].as_bool()), (0, Some(true)), "{v}");
    let (_, again) = wg(&h, &repo, &["memory", "import"]);
    assert_eq!(again["imported"], false);
    let (code, _) = wg(&h, &repo, &["memory", "nope"]);
    assert_eq!(code, 2);
}

#[test]
fn auth_key_writes_the_key_to_stdout_only() {
    let h = home("auth");
    let security = h.join("security");
    fs::write(
        &security,
        format!(
            "#!/bin/sh\n[ \"$1 $2 $4\" = \"find-generic-password -s -w\" ] && echo {SENTINEL}\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&security, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(h.join(".workgraph")).unwrap();
    fs::write(
        h.join(".workgraph/factory.toml"),
        format!("[tools]\nsecurity = \"{}\"\n\n[providers.subscription]\nkind = \"subscription\"\nmodels = {{ haiku = \"haiku\", sonnet = \"sonnet\", opus = \"opus\" }}\n\n[providers.anthropic-api]\nkind = \"anthropic-api\"\nkeychain_service = \"workgraph-test\"\nmodels = {{ haiku = \"h\", sonnet = \"s\", opus = \"o\" }}\n", security.display()),
    )
    .unwrap();
    // Claude Code runs the helper inside the child, where WG_AGENT is set; it must still answer.
    let out = wg_raw(&h, &h, &["auth", "key", "anthropic-api"], true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{SENTINEL}\n")
    );
    let bad = wg_raw(&h, &h, &["auth", "key", "subscription"], false);
    assert_eq!(bad.status.code(), Some(2));
    assert!(bad.stdout.is_empty(), "errors never go to the key pipe");
    assert!(String::from_utf8_lossy(&bad.stderr).contains("no keychain_service"));
    let walk = fs::read_dir(h.join(".workgraph")).unwrap().count();
    assert_eq!(walk, 1, "nothing but factory.toml was written");
}

#[test]
fn doctor_reports_each_check_and_refuses_user_allow_rules() {
    let h = home("doctor");
    let (code, v) = wg(&h, &h, &["doctor"]);
    assert_eq!(code, 0);
    assert_eq!(v["ok"], false, "no Haruspex pin yet: {v}");
    let names: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["check"].as_str().unwrap())
        .collect();
    for n in [
        "factory.toml",
        "user permissions",
        "claude",
        "haruspex pin",
        "dev pipeline",
    ] {
        assert!(names.contains(&n), "{n}");
    }
    fs::create_dir_all(h.join(".claude")).unwrap();
    fs::write(
        h.join(".claude/settings.json"),
        r#"{"permissions":{"allow":["Bash(rm:*)"]}}"#,
    )
    .unwrap();
    let (_, v) = wg(&h, &h, &["doctor"]);
    let perm = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "user permissions")
        .unwrap()
        .clone();
    assert_eq!(perm["ok"], false);
    let task = h.join("task.txt");
    fs::write(&task, "x").unwrap();
    let (code, v) = wg(&h, &h, &["run", "--task-file", task.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(
        v["error"].as_str().unwrap().contains("permissions.allow"),
        "{v}"
    );
    let (code, _) = wg(&h, &h, &["doctor", "--bogus"]);
    assert_eq!(code, 2);
}
