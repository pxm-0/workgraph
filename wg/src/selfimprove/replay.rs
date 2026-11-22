//! `wg eval replay` (design §12.4, Tier A): drives this build's factory engine through the real
//! kernel with recorded answers. A case (`eval/replay/<case>/case.json`, protected) holds the
//! structured replies by label, the check failures by label and attempt, and the expected terminal
//! status. Ports are cassettes: `claude` answers by label, checks by name and attempt, a static
//! Haruspex. Replay asserts invariants, not exact sequences (`replay_check.rs`), so an intended
//! behaviour change passes while a weakened gate does not.

use crate::budget::Ledger;
use crate::engine::driver::{self, Ran, Start};
use crate::engine::RunCtx;
use crate::journal::Journal;
use crate::ports::{CheckExec, Haruspex, Ports, Proc, RawOutput, Repo, Spawn};
use crate::state_dirs::Dirs;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub const REPO: &str = "eval/replay";

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub task: String,
    /// Check name → command.
    pub checks: BTreeMap<String, String>,
    /// Label → structured reply (a JSON string is printed raw, as an envelope would be).
    pub replies: BTreeMap<String, Value>,
    /// Check label → check name → tail per attempt (`null` passes).
    #[serde(default)]
    pub failures: BTreeMap<String, BTreeMap<String, Vec<Option<String>>>>,
    pub expect: Expect,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub status: String,
}

pub fn load(path: &Path) -> Result<Case, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The reply a label gets: exact, then without `!retry`/`!gate`, then the role's plain default.
pub fn reply(case: &Case, label: &str) -> Option<Value> {
    let base = label.trim_end_matches("!retry").trim_end_matches("!gate");
    if let Some(v) = case.replies.get(label).or_else(|| case.replies.get(base)) {
        return Some(v.clone());
    }
    let review = case.replies.get("review").cloned();
    if base.starts_with("review#") || base.starts_with("rereview#") {
        return review.or_else(|| Some(json!({ "passed": true, "criteria": [], "findings": [] })));
    }
    if base.starts_with("work:") {
        return Some(json!({ "summary": "done" }));
    }
    if base.contains(':') && base.contains('#') {
        return Some(json!({ "summary": "patched it" }));
    }
    if base == "learn" {
        return Some(json!({ "rules": [], "proposals": [] }));
    }
    None
}

struct CassetteProc<'a> {
    case: &'a Case,
}

impl Proc for CassetteProc<'_> {
    fn run(&self, s: &Spawn) -> RawOutput {
        let stdout = match reply(self.case, &s.label) {
            Some(Value::String(raw)) => raw,
            Some(v) => json!({ "type": "result", "subtype": "success", "is_error": false,
                "structured_output": v, "total_cost_usd": 0.0, "num_turns": 1, "session_id": "replay" })
            .to_string(),
            None => String::new(),
        };
        RawOutput {
            exit: Some(if stdout.is_empty() { 1 } else { 0 }),
            timed_out: false,
            stdout,
            stderr: String::new(),
        }
    }
}

struct CassetteChecks<'a> {
    case: &'a Case,
    seen: Mutex<BTreeMap<(String, String), usize>>,
}

impl CheckExec for CassetteChecks<'_> {
    fn run(&self, label: &str, cmd: &str, _t: Duration) -> Result<(i64, String), String> {
        let name = self
            .case
            .checks
            .iter()
            .find(|(_, c)| c.as_str() == cmd)
            .map(|(n, _)| n.clone())
            .ok_or_else(|| format!("no check runs {cmd:?}"))?;
        let mut seen = self.seen.lock().map_err(|_| "poisoned")?;
        let n = seen.entry((label.into(), name.clone())).or_insert(0);
        *n += 1;
        let tail = self
            .case
            .failures
            .get(label)
            .and_then(|by| by.get(&name))
            .and_then(|tails| tails.get(*n - 1).or(tails.last()).cloned())
            .flatten();
        Ok(match tail {
            Some(t) => (1, t),
            None => (0, "ok".into()),
        })
    }
}

struct CassetteRepo {
    files: Vec<String>,
}

impl Repo for CassetteRepo {
    fn changed(&self) -> Result<Vec<String>, String> {
        Ok(self.files.clone())
    }
    fn tree(&self) -> Result<String, String> {
        Ok("replay-tree".into())
    }
    fn diff(&self, _files: &[String]) -> Result<String, String> {
        Ok(String::new())
    }
}

struct CassetteHx {
    task: Value,
    project: BTreeMap<String, String>,
}

impl Haruspex for CassetteHx {
    fn gate(&self, _gate: &str) -> Result<Vec<String>, String> {
        Ok(vec![crate::engine::kernel::PENDING_APPROVAL.to_string()])
    }
    fn task(&self, _id: &str) -> Result<Value, String> {
        Ok(self.task.clone())
    }
    fn approvals(&self) -> Result<Value, String> {
        Ok(json!({ "ready_to_build": { "status": "pending", "by": null, "at": null } }))
    }
    fn project_commands(&self) -> Result<BTreeMap<String, String>, String> {
        Ok(self.project.clone())
    }
    fn managed_hash(&self) -> Result<String, String> {
        Ok("replay".into())
    }
}

/// What one replayed run left: its result line and its journal.
pub struct Replayed {
    pub result: Value,
    pub journal: Vec<Value>,
    pub memory: Option<Value>,
}

fn config(case: &Case) -> Result<crate::config::Config, String> {
    let mut text = String::new();
    for (n, c) in &case.checks {
        text.push_str(&format!(
            "[[check]]\nrepo = \"{REPO}\"\nname = {}\nproject = {}\nfactory = {}\n\n",
            toml::Value::String(n.clone()),
            toml::Value::String(c.clone()),
            toml::Value::String(c.clone())
        ));
    }
    crate::config::parse(&text)
}

/// Replays `case` with `step` in the scratch dir `home`.
pub fn run(case: &Case, home: &Path, step: driver::Step) -> Result<Replayed, String> {
    let dirs = Dirs {
        home: home.to_path_buf(),
    };
    let run_id = "r-replay0".to_string();
    let run_dir = dirs.run(&run_id);
    for d in [run_dir.join("logs"), run_dir.join("guard")] {
        crate::state_dirs::private_dir(&d)?;
    }
    let _tmp = crate::scratch::RunTmp::create(&dirs, &run_id)?;
    let cfg = config(case)?;
    let read = case.replies.get("read").cloned().unwrap_or(Value::Null);
    let units = read["units"].as_array().cloned().unwrap_or_default();
    let files: Vec<String> = units
        .iter()
        .flat_map(|u| u["files"].as_array().cloned().unwrap_or_default())
        .filter_map(|f| f.as_str().map(str::to_string))
        .collect();
    let criteria: Vec<Value> = read["criteria"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| json!({ "id": c["id"], "criterion": "x", "status": "unverified" }))
        .collect();
    let hx = CassetteHx {
        task: json!({ "id": read["taskId"], "acceptance_criteria": criteria,
                      "required_checks": case.checks.keys().collect::<Vec<_>>(), "failure_modes": [] }),
        project: case.checks.clone(),
    };
    let proc = CassetteProc { case };
    let checks = CassetteChecks {
        case,
        seen: Mutex::new(BTreeMap::new()),
    };
    let repo_port = CassetteRepo { files };
    let sandbox = crate::sandbox::client::NoSandbox("no sandbox in replay".into());
    let repo: PathBuf = dirs.work().join("replay-repo");
    crate::state_dirs::private_dir(&repo)?;
    let assets = crate::assets::Assets::compiled();
    let chain = crate::pipelines::chain(
        &assets,
        &["dev".to_string()],
        &crate::pipelines::RepoView::default(),
        cfg.max_calls_per_task,
    )?;
    let ctx = RunCtx {
        ports: Ports {
            proc: &proc,
            checks: &checks,
            repo: &repo_port,
            hx: &hx,
            sandbox: &sandbox,
        },
        cfg: &cfg,
        dirs: dirs.clone(),
        run_id: run_id.clone(),
        run_dir: run_dir.clone(),
        run_tmp: dirs.run_tmp(&run_id),
        repo,
        repo_name: REPO.into(),
        journal: Journal::open(&run_dir.join("journal.jsonl"), &run_id)?,
        ledger: Ledger::new(&dirs.ledger(), &run_id, cfg.max_calls_per_task),
        wg_exe: PathBuf::from("/opt/wg/bin/wg"),
        haruspex_cli: "/opt/haruspex/scripts/haruspex.py".into(),
        plugins: vec![],
        user: "replay".into(),
        replay: crate::claude::replay::Replay::none(),
        assets,
        shift: None,
    };
    let memory_path = dirs.memory("eval__replay");
    let start = Start {
        title: case.task.lines().next().unwrap_or_default().to_string(),
        task: case.task.clone(),
        decisions: vec![],
        resume_task: None,
        initialized_earlier: false,
        user_changes_before: vec![],
        memory: crate::memory_store::load(&memory_path),
        memory_path: memory_path.clone(),
        chain,
        start_branch: "main".into(),
        initializing: false,
        baseline_floor: None,
        tainted_earlier: false,
    };
    let ran: Ran = driver::run_task(&ctx, start, step)?;
    crate::cli::run::persist(&ctx, &dirs, &ran)?;
    let result = crate::report::output(&ctx, &ran)?;
    let journal = crate::journal::read(&run_dir.join("journal.jsonl"))?;
    let memory = std::fs::read_to_string(&memory_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    Ok(Replayed {
        result,
        journal,
        memory,
    })
}
