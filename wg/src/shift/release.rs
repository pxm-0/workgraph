//! The release line (design §7.3 step 10, §7.4, Phase 4): for a repo with `release = true`, each
//! task the owner verified (`wg approve` passed) whose PR has merged runs `release.toml` in a
//! worktree recreated from the merge commit. It re-runs Haruspex verify there with the pinned
//! commands, then writes notes and a plan, rehearses in the sim sandbox, evaluates
//! ready-to-release and stops at the owner's human stage. It never records a deployment, never
//! deploys and never touches a server: its only execution off the Mac is the sim sandbox.

use super::config::RepoCfg;
use super::intake;
use super::owner::VERIFIED;
use super::record::{self, TaskRecord};
use super::runctx;
use super::task::{stop_reason, tainted_earlier, Env};
use crate::engine::driver::{self, End, Start};
use crate::journal::now;
use crate::memory_store::Loaded;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use wg_factory::api::Memory;

pub const RELEASE_READY: &str = "wg:release-ready";

/// Verified tasks of `repo` with a PR, oldest claim first.
pub fn candidates(e: &Env, repo: &str) -> Result<Vec<TaskRecord>, String> {
    Ok(record::all(e.dirs)?
        .into_iter()
        .filter(|r| r.repo == repo && r.status == "verified" && r.pr.is_some())
        .collect())
}

/// Runs the release line for `repo`; returns how many releases it prepared or tried.
pub fn line(e: &Env, repo: &RepoCfg) -> Result<u32, String> {
    if !repo.release {
        return Ok(0);
    }
    let mut n = 0;
    for rec in candidates(e, &repo.name)? {
        if let Some(why) = stop_reason(&e.dirs.stop_file(), e.deadline) {
            e.journal.append("stopped", json!({ "why": why }))?;
            break;
        }
        let pr = rec.pr.expect("candidates have a PR");
        let view = e.gh.pr_view(&repo.name, pr)?;
        let merge = match (view.state.as_str(), &view.merge_commit) {
            ("MERGED", Some(sha)) => sha.clone(),
            (state, _) => {
                e.journal.append(
                    "release_waits",
                    json!({ "repo": repo.name, "issue": rec.issue, "pr": pr, "state": state }),
                )?;
                continue;
            }
        };
        n += 1;
        let issue = rec.issue;
        let result = prepare(e, repo, rec.clone(), &merge);
        let (status, label, comment, run) = match &result {
            Ok((End::AwaitingOwner { .. }, out)) => (
                "release-ready",
                Some(RELEASE_READY),
                format!(
                    "workgraph prepared the release of #{pr} at merge commit {merge}: notes, plan, a rehearsal in the sim sandbox, and ready-to-release has only the owner's approval left. It stops here; the factory never records a deployment and never deploys.\n\n{}",
                    fence(&notes(out))
                ),
                out["runId"].as_str().map(str::to_string),
            ),
            // The sandbox was unavailable or the shift stopped: try again next shift.
            Ok((End::Parked(why), out)) => {
                e.journal.append(
                    "release_parked",
                    json!({ "repo": repo.name, "issue": issue, "why": why, "run": out["runId"] }),
                )?;
                continue;
            }
            Ok((_, out)) => (
                "release-stuck",
                None,
                format!(
                    "workgraph could not prepare the release of #{pr}:\n\n{}",
                    fence(out["note"].as_str().unwrap_or("the release line stopped"))
                ),
                out["runId"].as_str().map(str::to_string),
            ),
            Err(why) => (
                "release-stuck",
                None,
                format!(
                    "workgraph could not prepare the release of #{pr}:\n\n{}",
                    fence(why)
                ),
                None,
            ),
        };
        if let Some(l) = label {
            e.gh.ensure_label(&repo.name, l)?;
            intake::relabel(e.dirs, e.gh, &repo.name, issue, &[l], &[VERIFIED])?;
        }
        e.gh.comment(&repo.name, issue, &comment)?;
        let latest = record::load(e.dirs, &repo.name, issue)?.unwrap_or(rec);
        record::save(
            e.dirs,
            &TaskRecord {
                status: status.into(),
                updated_at: now(),
                ..latest
            },
        )?;
        e.journal.append(
            "release_finished",
            json!({ "repo": repo.name, "issue": issue, "pr": pr, "merge": merge, "outcome": status, "run": run }),
        )?;
    }
    Ok(n)
}

fn fence(text: &str) -> String {
    format!("```text\n{}\n```", text.replace("```", "'''"))
}

fn notes(out: &Value) -> String {
    out["logDir"]
        .as_str()
        .and_then(|d| std::fs::read_to_string(Path::new(d).join("release-notes.md")).ok())
        .unwrap_or_else(|| "(no release notes)".into())
}

/// The release worktree, verify at the merge commit, then the release chain.
fn prepare(e: &Env, repo: &RepoCfg, rec: TaskRecord, merge: &str) -> Result<(End, Value), String> {
    let task = rec
        .task_id
        .clone()
        .ok_or("the task has no Haruspex task id")?;
    crate::ids::task_id(&task)?;
    let run_id = format!("r-{}", crate::tokens_store::random_hex(6)?);
    e.journal.append(
        "release_started",
        json!({ "repo": repo.name, "issue": rec.issue, "run": run_id, "merge": merge }),
    )?;
    let worktree = crate::git::release_worktree(
        Path::new(&repo.path),
        &e.dirs.worktrees(),
        &format!("{}/release-{}", record::slug(&repo.name), rec.issue),
        merge,
    )?;
    let tainted = repo.always_tainted || tainted_earlier(e.dirs, &rec.runs);
    verify(e, repo, &worktree, &task, tainted)?;
    let chain = crate::pipelines::chain(
        &crate::assets::Assets::compiled(),
        &["release".to_string()],
        &crate::pipelines::RepoView::of(repo),
        e.cfg.max_calls_per_task,
    )?;
    let place = runctx::Place {
        issue: rec.issue,
        branch: format!("release-{}", rec.issue),
        pipeline: "release".into(),
    };
    let replay = crate::claude::replay::Replay::none();
    let (ran, out) = runctx::with_ctx(e, repo, &worktree, &run_id, replay, &place, |ctx| {
        let start = Start {
            task: format!(
                "Prepare the release of {}#{} ({}), merged as {merge}.",
                repo.name,
                rec.issue,
                crate::ids::one_line(&rec.title, 200)
            ),
            title: rec.title.clone(),
            decisions: vec![],
            resume_task: Some(task.clone()),
            initialized_earlier: true,
            user_changes_before: vec![],
            // The release line never learns.
            memory: Loaded {
                memory: Memory {
                    next_id: 1,
                    ..Memory::default()
                },
                can_save: false,
                raw_models: Value::Null,
                warnings: vec![],
            },
            memory_path: e.dirs.memory(&record::slug(&repo.name)),
            chain,
            start_branch: String::new(),
            initializing: false,
            baseline_floor: None,
            tainted_earlier: tainted,
        };
        runctx::finish(
            ctx,
            driver::run_task(ctx, start, &wg_factory::engine::step)?,
        )
    })?;
    Ok((ran.end, out))
}

/// Haruspex verify at the merge commit with the owner-approved commands of the task's required
/// checks; a tainted task's go to the sandbox through `[repo.sandbox].verify` (design §9.5).
fn verify(
    e: &Env,
    repo: &RepoCfg,
    worktree: &Path,
    task: &str,
    tainted: bool,
) -> Result<(), String> {
    let pin = e.cfg.haruspex.clone().ok_or("no Haruspex CLI is pinned")?;
    let hx = crate::haruspex::cli::PinnedHaruspex::new(
        &pin,
        worktree,
        &[e.dirs.work(), e.dirs.state()],
    )?;
    let t = crate::ports::Haruspex::task(&hx, task)?;
    let required: Vec<String> = t["required_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_string))
        .collect();
    if required.is_empty() {
        return Err(format!("task {task} declares no required_checks"));
    }
    let mut pins = BTreeMap::new();
    for name in &required {
        let cmd = if tainted {
            repo.sandbox
                .as_ref()
                .and_then(|s| s.verify.get(name))
                .map(|c| c.replace("{task}", task))
                .ok_or("host verify refused for tainted task; add [repo.sandbox].verify or verify yourself")?
        } else {
            e.cfg
                .check(&repo.name, name)
                .map(|c| c.project.clone())
                .ok_or_else(|| format!("check {name} has no approved command for {}", repo.name))?
        };
        pins.insert(name.clone(), cmd);
    }
    let sandbox_project = if tainted {
        let text = std::fs::read_to_string(worktree.join(".haruspex/project.json"))
            .map_err(|e| format!("cannot read project.json: {e}"))?;
        let mut v: Value =
            serde_json::from_str(&text).map_err(|e| format!("project.json is not JSON: {e}"))?;
        for (name, cmd) in &pins {
            v["commands"][name] = json!(cmd);
        }
        Some(serde_json::to_string_pretty(&v).expect("serializes") + "\n")
    } else {
        None
    };
    crate::haruspex::reverify::at_merge(
        &hx.cli.to_string_lossy(),
        worktree,
        &pins,
        sandbox_project.as_deref(),
    )
}
