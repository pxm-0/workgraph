//! The run context of one shift run in a worktree: its directories, the pinned Haruspex, the sim
//! sandbox, the journal and ledger, and the shift's stop conditions. Shared by task runs (dev and
//! its later stages) and the release line.

use super::config::RepoCfg;
use super::task::{stop_reason, Env};
use crate::budget::Ledger;
use crate::claude::replay::Replay;
use crate::engine::driver::Ran;
use crate::engine::{RunCtx, ShiftCtx};
use crate::haruspex::cli::PinnedHaruspex;
use crate::journal::Journal;
use crate::ports::{GitRepo, Ports, RealProc, ShellChecks};
use crate::state_dirs::private_dir;
use serde_json::{json, Value};
use std::path::Path;

/// Where a run works: the issue it is for, its branch (the task branch, or the release
/// worktree's detached merge commit), and the chain's name for the journal.
pub struct Place {
    pub issue: u64,
    pub branch: String,
    pub pipeline: String,
}

/// Builds the run's context in `worktree` and hands it to `f`.
pub fn with_ctx<R>(
    e: &Env,
    repo: &RepoCfg,
    worktree: &Path,
    run_id: &str,
    replay: Replay,
    place: &Place,
    f: impl FnOnce(&RunCtx) -> Result<R, String>,
) -> Result<R, String> {
    let dirs = e.dirs;
    let run_dir = dirs.run(run_id);
    for d in [run_dir.join("logs"), run_dir.join("guard")] {
        private_dir(&d)?;
    }
    let _tmp = crate::scratch::RunTmp::create(dirs, run_id)?;
    let pin = e.cfg.haruspex.clone().ok_or("no Haruspex CLI is pinned")?;
    let hx = PinnedHaruspex::new(&pin, worktree, &[dirs.work(), dirs.state()])?;
    let stop_file = dirs.stop_file();
    let deadline = e.deadline;
    let sandbox = crate::sandbox::client::SimSandbox {
        cfg: e.cfg.clone(),
        repo: repo.sandbox.clone(),
        root: worktree.to_path_buf(),
        status: e.sandbox.clone(),
    };
    let assets = crate::assets::Assets::compiled();
    let ctx = RunCtx {
        ports: Ports {
            proc: &RealProc,
            checks: &ShellChecks {
                root: worktree.to_path_buf(),
                prefix: repo.factory_compose.clone(),
            },
            repo: &GitRepo {
                root: worktree.to_path_buf(),
            },
            hx: &hx,
            sandbox: &sandbox,
        },
        cfg: e.cfg,
        dirs: dirs.clone(),
        run_id: run_id.to_string(),
        run_dir: run_dir.clone(),
        run_tmp: dirs.run_tmp(run_id),
        repo: worktree.to_path_buf(),
        repo_name: repo.name.clone(),
        journal: Journal::open(&run_dir.join("journal.jsonl"), run_id)?,
        ledger: Ledger::new(&dirs.ledger(), run_id, e.cfg.max_calls_per_task),
        wg_exe: std::env::current_exe().map_err(|e| format!("cannot find wg itself: {e}"))?,
        haruspex_cli: hx.cli.to_string_lossy().into_owned(),
        plugins: crate::claude::auth::plugin_profile(&dirs.home, &e.cfg.keep_plugins),
        user: std::env::var("USER").unwrap_or_default(),
        replay,
        assets,
        shift: Some(ShiftCtx {
            gh: e.gh.clone(),
            issue: place.issue,
            branch: place.branch.clone(),
            base: repo.default_branch.clone(),
            attribution: e.cfg.shift.attribution.clone(),
            shift_id: e.shift_id.to_string(),
            max_parallel: e.cfg.shift.max_parallel_units as usize,
            stop: Box::new(move || stop_reason(&stop_file, deadline)),
        }),
    };
    ctx.journal.append(
        "run_started",
        json!({ "repo": worktree, "slug": super::record::slug(&repo.name), "binary": crate::claude::replay::binary_hash(),
                "version": env!("CARGO_PKG_VERSION"), "assets": ctx.assets.hash(), "pipeline": place.pipeline,
                "shift": e.shift_id, "task": format!("{}#{}", repo.name, place.issue),
                "replay_from": ctx.replay.from, "replayable": ctx.replay.len() }),
    )?;
    f(&ctx)
}

/// Persists the run's tokens and writes its output and report.
pub fn finish(ctx: &RunCtx, ran: Ran) -> Result<(Ran, Value), String> {
    crate::cli::run::persist(ctx, &ctx.dirs, &ran)?;
    let out = crate::report::output(ctx, &ran)?;
    std::fs::write(ctx.run_dir.join("output.json"), out.to_string())
        .map_err(|e| format!("cannot write the run output: {e}"))?;
    std::fs::write(ctx.run_dir.join("report.md"), crate::report::markdown(&out))
        .map_err(|e| format!("cannot write the report: {e}"))?;
    ctx.journal.append(
        "run_finished",
        json!({ "status": out["status"], "pr": ran.published }),
    )?;
    Ok((ran, out))
}
