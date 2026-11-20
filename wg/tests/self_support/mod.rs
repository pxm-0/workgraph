//! Self-lane fixtures (design §15.4): a small copy of the factory repo with a local bare origin, a
//! fake `gh` (fixtures/fake-gh.py) serving PRs, check-runs and release runs from a JSON state, and
//! a home whose factory.toml names both. Self PRs are made the way the lane makes them: a
//! `wg-self/*` branch whose commit carries a `Workgraph-Run` trailer the self-ledger records.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use wg::selfimprove::ledger::Ledger;

pub const REPO: &str = "pxm-0/workgraph";
pub const WG: &str = env!("CARGO_BIN_EXE_wg");

pub struct Fx {
    pub base: PathBuf,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub origin: PathBuf,
    pub state: PathBuf,
}

pub fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "setup failed: {script}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn real(rel: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel)).unwrap()
}

/// Every required check-run, passing, from the workflow that defines it.
pub fn green() -> Value {
    json!(wg::selfimprove::protected::compiled()
        .required_checks
        .iter()
        .map(|(wf, job)| json!({ "name": job, "workflow": wf }))
        .collect::<Vec<_>>())
}

pub fn fx(name: &str, extra_toml: &str) -> Fx {
    let base = std::env::temp_dir().join(format!("wg-self-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let home = base.join("home");
    fs::create_dir_all(home.join(".workgraph")).unwrap();
    let gh = base.join("fake-gh.py");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.py"),
        &gh,
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let repo = base.join("workgraph");
    let origin = base.join("workgraph.git");
    for (rel, text) in [
        ("config/protected.toml", real("config/protected.toml")),
        ("pipelines/dev.toml", real("pipelines/dev.toml")),
        ("prompts/work.md", real("prompts/work.md")),
        (
            "prompts/self-review.md",
            "Review the self change.\n".to_string(),
        ),
        ("docs/a.md", "# a\n".into()),
        ("README.md", "# workgraph\n".into()),
        ("Cargo.toml", "[workspace]\n".into()),
        ("wg/src/gh.rs", "// kernel\n".into()),
        ("wg/tests/flow.rs", "// flow tests\n".into()),
        ("eval/replay/happy-path/case.json", "{}\n".into()),
        (
            "wg-factory/Cargo.toml",
            "[package]\nname = \"wg-factory\"\n".into(),
        ),
        ("wg-factory/src/engine/mod.rs", "pub fn step() {}\n".into()),
        ("wg-factory/src/plan/mod.rs", "pub fn plan() {}\n".into()),
        ("wg-factory/tests/stages.rs", "#[test]\nfn t() {}\n".into()),
    ] {
        let p = repo.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fs::create_dir_all(&origin).unwrap();
    sh(&origin, "git init -q --bare -b main");
    sh(&repo, &format!(
        "git init -q -b main && git config user.name t && git config user.email t@example.invalid \
         && git add -A && git commit -qm init && git remote add origin {} && git push -q origin main",
        origin.display()
    ));
    let toml = format!(
        "[tools]\ngh = \"{}\"\n\n[self_improve]\nenabled = true\n{extra_toml}\n\n\
         [[repo]]\nname = \"{REPO}\"\npath = \"{}\"\npipelines = [\"self\"]\nalways_tainted = true\n\
         [repo.sandbox]\nimage = \"docker.io/library/rust@sha256:{}\"\nchecks = {{ test = \"cargo test\" }}\n",
        gh.display(),
        repo.display(),
        "a".repeat(64)
    );
    fs::write(home.join(".workgraph/factory.toml"), toml).unwrap();
    let state = base.join("gh-state.json");
    fs::write(
        &state,
        json!({ "origins": { REPO: origin }, "issues": {}, "prs": [], "check_runs": green() })
            .to_string(),
    )
    .unwrap();
    Fx {
        base,
        home,
        repo: repo.canonicalize().unwrap(),
        origin: origin.canonicalize().unwrap(),
        state,
    }
}

impl Fx {
    pub fn gh_state(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(&self.state).unwrap()).unwrap()
    }

    pub fn set_gh(&self, f: impl FnOnce(&mut Value)) {
        let mut s = self.gh_state();
        f(&mut s);
        fs::write(&self.state, s.to_string()).unwrap();
    }

    pub fn ledger(&self) -> Ledger {
        Ledger::at(&self.home.join(".workgraph/self-ledger.jsonl"))
    }

    /// A self PR: `change` runs in a fresh clone on `wg-self/<slug>`; the commit carries a
    /// Workgraph-Run trailer the self-ledger records (unless `factory` is false: the owner's push).
    pub fn pr(&self, slug: &str, change: &str, factory: bool) -> (u64, String) {
        let work = self.base.join(format!("work-{slug}"));
        let _ = fs::remove_dir_all(&work);
        sh(
            &self.base,
            &format!("git clone -q {} {}", self.origin.display(), work.display()),
        );
        let run = format!(
            "r-{:06x}",
            self.gh_state()["prs"].as_array().map_or(0, |p| p.len()) + 1
        );
        let trailer = if factory {
            format!("\n\nWorkgraph-Run: {run}")
        } else {
            String::new()
        };
        sh(&work, &format!(
            "git config user.name t && git config user.email t@example.invalid && git checkout -q -b wg-self/{slug} \
             && {change} && git add -A && git commit -qm 'feat: {slug}{trailer}' && git push -q origin wg-self/{slug}"
        ));
        let head = sh(&work, "git rev-parse HEAD");
        let mut n = 0;
        self.set_gh(|s| {
            let prs = s["prs"].as_array_mut().unwrap();
            n = 200 + prs.len() as u64;
            prs.push(json!({ "repo": REPO, "number": n, "url": format!("https://github.com/{REPO}/pull/{n}"),
                "state": "OPEN", "headRefName": format!("wg-self/{slug}"), "base": "main",
                "title": format!("feat: {slug}"), "body": "", "draft": false, "labels": ["wg:self"] }));
        });
        if factory {
            self.ledger()
                .append(
                    "authored",
                    json!({ "repo": REPO, "pr": n, "head": head, "run": run }),
                )
                .unwrap();
        }
        (n, head)
    }

    pub fn wg(&self, args: &[&str]) -> (i32, Value) {
        let out = Command::new(WG)
            .args(args)
            .env("HOME", &self.home)
            .env("FAKE_GH_STATE", &self.state)
            .env_remove("WG_AGENT")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let v = serde_json::from_str(text.trim()).unwrap_or_else(|_| {
            panic!(
                "not one JSON line: {text}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), v)
    }

    pub fn gate(&self, pr: u64) -> Value {
        let (code, v) = self.wg(&["self", "gate", "--pr", &pr.to_string()]);
        assert_eq!(code, 0, "{v}");
        v
    }

    /// The owner closes `pr` (so it no longer counts as an open self PR).
    pub fn close(&self, pr: u64) {
        self.set_gh(|s| {
            for p in s["prs"].as_array_mut().unwrap() {
                if p["number"] == pr {
                    p["state"] = json!("CLOSED");
                }
            }
        });
    }

    pub fn labels(&self, pr: u64) -> Vec<String> {
        self.gh_state()["prs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["number"] == pr)
            .map(|p| serde_json::from_value(p["labels"].clone()).unwrap())
            .unwrap_or_default()
    }

    /// Merging on: the owner's `--enable-merge` after enough shadow decisions.
    pub fn enable_merge(&self) {
        for i in 0..5 {
            self.ledger()
                .append(
                    "decision",
                    json!({ "pr": 900 + i, "outcome": "shadow", "shadow": true }),
                )
                .unwrap();
        }
        let (code, v) = self.wg(&["self", "resume", "--enable-merge"]);
        assert_eq!((code, v["merge_enabled"].clone()), (0, json!(true)), "{v}");
    }
}
