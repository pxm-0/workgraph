//! Shift fixtures (design §15.3, §15.4): toy repos with a local bare origin, a scripted `claude`
//! (fixtures/fake-claude.py), a fake `gh` serving a JSON state (fixtures/fake-gh.py) and the real,
//! pinned Haruspex CLI. Tests skip without HARUSPEX_CLI=/abs/haruspex.py, as e2e.rs does.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};

pub struct Fx {
    pub base: PathBuf,
    pub home: PathBuf,
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

pub fn cli() -> Option<String> {
    let c = std::env::var("HARUSPEX_CLI").ok();
    if c.is_none() {
        eprintln!("skipped: set HARUSPEX_CLI=/abs/path/haruspex.py to run the shift tests");
    }
    c
}

fn copy_script(base: &Path, name: &str) -> PathBuf {
    let to = base.join(name);
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
        &to,
    )
    .unwrap();
    fs::set_permissions(&to, fs::Permissions::from_mode(0o755)).unwrap();
    to
}

/// A toy repo `name` (`owner/repo`) with its bare origin, pushed.
fn toy(base: &Path, name: &str) -> (PathBuf, PathBuf) {
    let short = name.replace('/', "_");
    let origin = base.join(format!("{short}.git"));
    let repo = base.join(&short);
    fs::create_dir_all(&origin).unwrap();
    fs::create_dir_all(repo.join(".devcontainer")).unwrap();
    sh(&origin, "git init -q --bare -b main");
    fs::write(repo.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    fs::write(
        repo.join("check.py"),
        "import glob, runpy\nfor f in sorted(glob.glob('test_*.py')):\n    runpy.run_path(f)\n",
    )
    .unwrap();
    // A coverage build's `wg guard` hook leaves profile files where it runs.
    fs::write(repo.join(".gitignore"), "__pycache__/\n*.profraw\n").unwrap();
    fs::write(
        repo.join("compose.dev.yml"),
        "services:\n  workspace:\n    image: x\n",
    )
    .unwrap();
    fs::write(repo.join(".devcontainer/devcontainer.json"), "{}\n").unwrap();
    sh(&repo, &format!(
        "git init -q -b main && git config user.name t && git config user.email t@example.invalid \
         && git add -A && git commit -qm init && git remote add origin {} && git push -q origin main",
        origin.display()
    ));
    (repo.canonicalize().unwrap(), origin.canonicalize().unwrap())
}

/// A home with factory.toml, a shift window open from now for 23 h 59 min, and `repos`.
pub fn fx(name: &str, cli: &str, repos: &[&str], extra: &str) -> Fx {
    let base = std::env::temp_dir().join(format!("wg-shift-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let home = base.join("home");
    fs::create_dir_all(home.join(".workgraph")).unwrap();
    let claude = copy_script(&base, "fake-claude.py");
    let gh = copy_script(&base, "fake-gh.py");
    let cli = Path::new(cli).canonicalize().unwrap();
    let sha = wg::haruspex::cli::sha256_file(&cli).unwrap();
    let now = sh(&base, "date +%H:%M");
    let (h, m): (u32, u32) = (now[..2].parse().unwrap(), now[3..].parse().unwrap());
    let until = (h * 60 + m + 1439) % 1440;
    let mut toml = format!(
        "[tools]\nclaude = \"{}\"\ngh = \"{}\"\n\n[haruspex]\ncli = \"{}\"\nsha256 = \"{sha}\"\n\n\
         [shift]\nstart = \"{now}\"\nuntil = \"{:02}:{:02}\"\n{extra}\n\n[intake]\ntrusted_actors = [\"pxm-0\"]\n\n",
        claude.display(), gh.display(), cli.display(), until / 60, until % 60
    );
    let mut origins = serde_json::Map::new();
    let mut issues = serde_json::Map::new();
    for r in repos {
        let (repo, origin) = toy(&base, r);
        toml.push_str(&format!(
            "[[check]]\nrepo = \"{r}\"\nname = \"test\"\nproject = \"python3 check.py\"\nfactory = \"python3 check.py\"\n\n\
             [[repo]]\nname = \"{r}\"\npath = \"{}\"\ninstall = \"true\"\n\n",
            repo.display()
        ));
        origins.insert(r.to_string(), json!(origin));
        issues.insert(r.to_string(), json!({}));
    }
    fs::write(home.join(".workgraph/factory.toml"), toml).unwrap();
    let state = base.join("gh-state.json");
    fs::write(
        &state,
        json!({ "origins": origins, "issues": issues, "prs": [] }).to_string(),
    )
    .unwrap();
    Fx { base, home, state }
}

impl Fx {
    pub fn repo(&self, name: &str) -> PathBuf {
        self.base.join(name.replace('/', "_"))
    }

    pub fn origin(&self, name: &str) -> PathBuf {
        self.base.join(format!("{}.git", name.replace('/', "_")))
    }

    /// An open issue labelled `wg:queued` by `labeller`.
    pub fn issue(&self, repo: &str, n: u64, title: &str, body: &str, author: &str, labeller: &str) {
        let mut s = self.state();
        s["issues"][repo][n.to_string()] = json!({
            "number": n, "title": title, "body": body, "author": { "login": author },
            "labels": [{ "name": "wg:queued" }], "url": format!("https://github.com/{repo}/issues/{n}"),
            "createdAt": format!("2026-11-01T10:00:{n:02}Z"),
            "events": [{ "label": "wg:queued", "actor": labeller, "at": "2026-11-01T11:00:00Z" }],
            "edits": [],
        });
        self.set_state(&s);
    }

    pub fn state(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(&self.state).unwrap()).unwrap()
    }

    pub fn set_state(&self, v: &Value) {
        fs::write(&self.state, v.to_string()).unwrap();
    }

    pub fn labels(&self, repo: &str, n: u64) -> Vec<String> {
        self.state()["issues"][repo][n.to_string()]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn comments(&self, repo: &str, n: u64) -> Vec<String> {
        self.state()["issues"][repo][n.to_string()]["comments"]
            .as_array()
            .map(|a| a.iter().map(|c| c.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    pub fn prs(&self) -> Vec<Value> {
        self.state()["prs"].as_array().unwrap().clone()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wg"));
        c.args(args)
            .current_dir(&self.base)
            .env("HOME", &self.home)
            .env("FAKE_GH_STATE", &self.state)
            // `<base>/bin` first: a test may put a fake `docker` there.
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.base.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env_remove("WG_AGENT");
        c
    }

    pub fn wg(&self, args: &[&str]) -> (i32, Value) {
        let out: Output = self.command(args).output().unwrap();
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

    pub fn spawn(&self, args: &[&str]) -> Child {
        self.command(args)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    pub fn shift(&self) -> Value {
        let (code, out) = self.wg(&["shift", "run"]);
        assert_eq!(code, 0, "{out}");
        out
    }

    pub fn report(&self, id: &str) -> Value {
        let p = self
            .home
            .join(".workgraph/shifts")
            .join(id)
            .join("report.json");
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    pub fn journal(&self, path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub fn run_journal(&self, run: &str) -> Vec<Value> {
        self.journal(
            &self
                .home
                .join(".workgraph/runs")
                .join(run)
                .join("journal.jsonl"),
        )
    }

    pub fn ledger(&self) -> Vec<Value> {
        self.journal(&self.home.join(".workgraph/cost.jsonl"))
    }

    pub fn toml(&self) -> PathBuf {
        self.home.join(".workgraph/factory.toml")
    }

    /// Appends tables to factory.toml (providers, auth).
    pub fn append_toml(&self, text: &str) {
        let t = fs::read_to_string(self.toml()).unwrap();
        fs::write(self.toml(), format!("{t}\n{text}\n")).unwrap();
    }

    /// Sets `key = value` (raw TOML) in the `[[repo]]` table of `repo`.
    pub fn set_repo(&self, repo: &str, key: &str, value: &str) {
        let text = fs::read_to_string(self.toml()).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let name = format!("name = \"{repo}\"");
        let start = (1..lines.len())
            .find(|&i| lines[i] == name && lines[i - 1] == "[[repo]]")
            .unwrap();
        let end = (start..lines.len())
            .find(|&i| lines[i].is_empty())
            .unwrap_or(lines.len());
        let line = format!("{key} = {value}");
        match (start..end).find(|&i| lines[i].starts_with(&format!("{key} = "))) {
            Some(i) => lines[i] = line,
            None => lines.insert(end, line),
        }
        fs::write(self.toml(), lines.join("\n") + "\n").unwrap();
    }

    /// Runs `script` in `repo`'s checkout, then pushes main to its origin.
    pub fn push(&self, repo: &str, script: &str) {
        sh(
            &self.repo(repo),
            &format!("{script} && git commit -qm change && git push -q origin main"),
        );
    }

    pub fn record_path(&self, repo: &str, n: u64) -> PathBuf {
        self.home
            .join(".workgraph/tasks")
            .join(format!("{}#{n}.json", repo.replace('/', "__")))
    }

    pub fn record(&self, repo: &str, n: u64) -> Value {
        let p = self
            .home
            .join(".workgraph/tasks")
            .join(format!("{}#{n}.json", repo.replace('/', "__")));
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }
}
