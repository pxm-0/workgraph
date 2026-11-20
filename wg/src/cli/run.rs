//! `wg run --experimental` (Phase 1a): one task in the current repo through the dev pipeline.

use super::args::{not_an_agent, parse};
use super::doctor::user_settings_problem;
use crate::budget::Ledger;
use crate::config::{self, Config};
use crate::engine::driver::{self, Start};
use crate::engine::RunCtx;
use crate::haruspex::cli::PinnedHaruspex;
use crate::journal::Journal;
use crate::ports::{GitRepo, Ports, RealProc, ShellChecks};
use crate::state_dirs::{private_dir, Dirs};
use crate::{git, ids, memory_store, tokens_store};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use wg_factory::api::Decision;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDecision {
    id: String,
    #[serde(default)]
    answer: Option<String>,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &str, what: &str) -> Result<T, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {what} {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{what} {path} is not valid: {e}"))
}

/// Decisions without an answer are dropped, as run.js did; ids and answers are capped.
pub fn decisions(path: Option<&str>) -> Result<Vec<Decision>, String> {
    let Some(p) = path else { return Ok(vec![]) };
    let raw: Vec<WireDecision> = read_json(p, "the decisions file")?;
    Ok(raw
        .into_iter()
        .filter_map(|d| {
            let answer = d.answer?.trim().to_string();
            let id = ids::one_line(d.id.trim(), 32);
            (!id.is_empty() && !answer.is_empty()).then(|| Decision {
                id,
                answer: ids::one_line(&answer, 2000),
            })
        })
        .collect())
}

/// The canonical repo root; `wg run` must start there.
pub fn repo_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("no current directory: {e}"))?;
    let top = PathBuf::from(git::toplevel(&cwd)?);
    let (cwd, top) = (
        cwd.canonicalize().map_err(|e| e.to_string())?,
        top.canonicalize().map_err(|e| e.to_string())?,
    );
    if cwd != top {
        return Err(format!("run wg from the repo root ({})", top.display()));
    }
    Ok(top)
}

pub fn load_config(dirs: &Dirs) -> Result<Config, String> {
    config::load(&dirs.config())
}

pub fn run(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &["experimental", "initialized"])?;
    a.only(&[
        "experimental",
        "initialized",
        "task-file",
        "task-id",
        "user-changes-file",
        "decisions-file",
        "run-id",
    ])?;
    not_an_agent("wg run")?;
    if !a.on("experimental") {
        return Err("wg run is Phase 1a: pass --experimental (run.js stays the default)".into());
    }
    let task = std::fs::read_to_string(a.need("task-file")?)
        .map_err(|e| format!("cannot read the task file: {e}"))?;
    let task = task.trim().to_string();
    if task.is_empty() {
        return Err("work: no task given".into());
    }
    let run_id = match a.get("run-id") {
        Some(r) => {
            ids::run_id(r)?;
            r.to_string()
        }
        None => format!("r-{}", tokens_store::random_hex(6)?),
    };
    let resume_task = a.get("task-id").map(str::to_string);
    if let Some(t) = &resume_task {
        ids::task_id(t)?;
    }
    let dirs = Dirs::from_env()?;
    let cfg = load_config(&dirs)?;
    if let Some(problem) = user_settings_problem(&dirs.claude_settings())? {
        return Err(problem);
    }
    let pin = cfg
        .haruspex
        .clone()
        .ok_or("no Haruspex CLI is pinned: run wg config pin-haruspex --cli <absolute path>")?;
    // In place the chain is dev only: the skill commits through wg finalize --run, no publish.
    let assets = crate::assets::Assets::compiled();
    let chain = crate::pipelines::chain(
        &assets,
        &["dev".to_string()],
        &crate::pipelines::RepoView::default(),
        cfg.max_calls_per_task,
    )?;
    let repo = repo_root()?;
    let run_dir = dirs.run(&run_id);
    if run_dir.exists() {
        return Err(format!("run {run_id} already exists"));
    }
    for d in [
        run_dir.join("logs"),
        run_dir.join("guard"),
        dirs.run_tmp(&run_id),
    ] {
        private_dir(&d)?;
    }
    let hx = PinnedHaruspex::new(&pin, &repo, &[dirs.work(), dirs.state()])?;
    let slug = git::slug(&repo);
    let sandbox = crate::sandbox::client::SimSandbox {
        cfg: cfg.clone(),
        repo: cfg
            .repo(&git::repo_name(&slug))
            .and_then(|r| r.sandbox.clone()),
        root: repo.clone(),
        status: Default::default(),
    };
    let memory_path = dirs.memory(&slug);
    let imported = memory_store::import(&repo.join(".claude/work-memory.json"), &memory_path)?;
    let _lock = memory_store::Lock::take(&memory_path)?;
    let user_changes: Vec<String> = match a.get("user-changes-file") {
        Some(p) => read_json(p, "the user-changes file")?,
        None => git::dirty_in(&repo)?,
    };
    let ctx = RunCtx {
        ports: Ports {
            proc: &RealProc,
            checks: &ShellChecks {
                root: repo.clone(),
                prefix: None,
            },
            repo: &GitRepo { root: repo.clone() },
            hx: &hx,
            sandbox: &sandbox,
        },
        cfg: &cfg,
        run_id: run_id.clone(),
        run_tmp: dirs.run_tmp(&run_id),
        repo: repo.clone(),
        repo_name: git::repo_name(&slug),
        journal: Journal::open(&run_dir.join("journal.jsonl"), &run_id)?,
        ledger: Ledger::new(&dirs.ledger(), &run_id, cfg.max_calls_per_task),
        wg_exe: std::env::current_exe().map_err(|e| format!("cannot find wg itself: {e}"))?,
        haruspex_cli: hx.cli.to_string_lossy().into_owned(),
        plugins: crate::claude::auth::plugin_profile(&dirs.home, &cfg.keep_plugins),
        user: std::env::var("USER").unwrap_or_default(),
        replay: crate::claude::replay::Replay::none(),
        assets: assets.clone(),
        shift: None,
        run_dir: run_dir.clone(),
        dirs: dirs.clone(),
    };
    ctx.journal.append(
        "run_started",
        json!({ "repo": repo, "slug": slug, "binary": crate::claude::replay::binary_hash(), "version": env!("CARGO_PKG_VERSION"),
                "assets": assets.hash(), "pipeline": "dev", "memory_import": imported }),
    )?;
    let start = Start {
        title: task.lines().next().unwrap_or_default().to_string(),
        task,
        decisions: decisions(a.get("decisions-file"))?,
        resume_task,
        initialized_earlier: a.on("initialized"),
        user_changes_before: user_changes,
        memory: memory_store::load(&memory_path),
        memory_path,
        chain,
        start_branch: git::branch(&repo)?,
        initializing: !repo.join(".haruspex").exists(),
        baseline_floor: None,
        tainted_earlier: false,
    };
    let ran = driver::run_task(&ctx, start, &wg_factory::engine::step)?;
    persist(&ctx, &dirs, &ran)?;
    let out = crate::report::output(&ctx, &ran)?;
    std::fs::write(run_dir.join("report.md"), crate::report::markdown(&out))
        .map_err(|e| format!("cannot write the report: {e}"))?;
    ctx.journal
        .append("run_finished", json!({ "status": out["status"] }))?;
    Ok(out)
}

/// The tokens and what finalize must re-check, MAC'd under the agent-denied state dir.
pub fn persist(ctx: &RunCtx, dirs: &Dirs, ran: &driver::Ran) -> Result<(), String> {
    let k = &ran.kernel;
    let exclude = match &ran.end {
        driver::End::Finish(o) => o.committable.clone(),
        _ => k.user_changes.clone(),
    };
    let meta = tokens_store::RunMeta {
        run: ctx.run_id.clone(),
        repo: ctx.repo.to_string_lossy().into_owned(),
        task_id: k.task_id(),
        gate: k.gate.clone(),
        exclude,
        pins: k.pins.clone(),
        contract: k.contract.clone().unwrap_or_default(),
        approvals: k.approvals.clone(),
        open_escalations: k.open().iter().map(|e| e.id.clone()).collect(),
        decided: k.decided.clone(),
        tainted: k.tainted,
        risky: k.risky.clone(),
    };
    let key = tokens_store::key(&dirs.key())?;
    tokens_store::save(
        &ctx.run_dir.join("tokens.jsonl"),
        &key,
        &meta,
        &k.checks,
        k.review_pass.as_ref(),
        &k.sandbox_runs,
    )
}

/// The run dir of an existing run, checked to belong to `repo`.
pub fn run_dir_for(dirs: &Dirs, run: &str) -> Result<PathBuf, String> {
    ids::run_id(run)?;
    let d = dirs.run(run);
    if !Path::new(&d).join("tokens.jsonl").exists() {
        return Err(format!("no finished run {run}"));
    }
    Ok(d)
}
