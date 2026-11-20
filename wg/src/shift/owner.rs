//! The owner's commands for shift work (design §9.5, §11.2): `wg decide`, `wg approval-summary`
//! and `wg approve`. Each runs only from the owner's session (never inside an agent), and only
//! `approve` builds a `HumanApproval`, inside `cli::finalize::finalize_at`.

use super::intake;
use super::record::{self, TaskRecord};
use crate::config::Config;
use crate::gh::Gh;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::path::Path;

pub const NEEDS_DECISION: &str = "wg:needs-decision";
pub const PR_OPEN: &str = "wg:pr-open";
pub const VERIFIED: &str = "wg:verified";

fn repo_cfg<'a>(cfg: &'a Config, repo: &str) -> Result<&'a super::config::RepoCfg, String> {
    cfg.repo(repo)
        .ok_or_else(|| format!("{repo} is not a [[repo]] in factory.toml"))
}

/// Stores the owner's answers for an issue and queues it again. Answers merge by id, newest wins;
/// they are read only from this file, never from issue comments (design §11.2).
pub fn decide(
    dirs: &Dirs,
    cfg: &Config,
    gh: &Gh,
    repo: &str,
    issue: u64,
    file: &str,
) -> Result<Value, String> {
    repo_cfg(cfg, repo)?;
    let new = crate::cli::run::decisions(Some(file))?;
    if new.is_empty() {
        return Err("the decisions file holds no answered decision".into());
    }
    let path = dirs.decisions(&record::slug(repo), issue);
    let mut all: Vec<(String, String)> = if path.exists() {
        crate::cli::run::decisions(Some(&path.to_string_lossy()))?
            .into_iter()
            .map(|d| (d.id, d.answer))
            .collect()
    } else {
        vec![]
    };
    all.retain(|(id, _)| !new.iter().any(|d| &d.id == id));
    all.extend(new.iter().map(|d| (d.id.clone(), d.answer.clone())));
    let body: Vec<Value> = all
        .iter()
        .map(|(id, a)| json!({ "id": id, "answer": a }))
        .collect();
    record::write_atomic(
        &path,
        &(serde_json::to_string_pretty(&body).expect("serializes") + "\n"),
    )?;
    intake::relabel(
        dirs,
        gh,
        repo,
        issue,
        &[&cfg.shift.label],
        &[NEEDS_DECISION],
    )?;
    intake::ledger(
        dirs,
        json!({ "kind": "decision", "repo": repo, "issue": issue,
        "ids": new.iter().map(|d| d.id.clone()).collect::<Vec<_>>(), "at": crate::journal::now() }),
    )?;
    if let Some(rec) = record::load(dirs, repo, issue)? {
        record::save(
            dirs,
            &TaskRecord {
                status: "queued".into(),
                updated_at: crate::journal::now(),
                ..rec
            },
        )?;
    }
    Ok(
        json!({ "repo": repo, "issue": issue, "decided": new.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
               "stored": path, "label": cfg.shift.label }),
    )
}

fn last_output(dirs: &Dirs, rec: &TaskRecord) -> Result<(String, Value), String> {
    let run = rec.runs.last().ok_or("the task has no run")?;
    let text = std::fs::read_to_string(dirs.run(run).join("output.json"))
        .map_err(|e| format!("run {run} has no output: {e}"))?;
    Ok((
        run.clone(),
        serde_json::from_str(&text).map_err(|e| format!("run {run} output: {e}"))?,
    ))
}

/// What the owner sees before approving a shift PR (design §7.5 "Approvals").
pub fn summary(dirs: &Dirs, cfg: &Config, gh: &Gh, repo: &str, pr: u64) -> Result<Value, String> {
    repo_cfg(cfg, repo)?;
    let rec = record::by_pr(dirs, repo, pr)?;
    let (run, out) = last_output(dirs, &rec)?;
    let view = gh.pr_view(repo, pr)?;
    let head = crate::git::head(Path::new(&rec.worktree))?;
    Ok(json!({
        "repo": repo, "issue": rec.issue, "pr": pr, "url": view.url, "state": view.state, "title": rec.title,
        "headMatches": head == view.head_oid, "runId": run, "taskId": out["taskId"],
        "verified": out["verified"], "reviewed": out["reviewed"], "review": out["review"],
        "checks": out["checks"], "checksAlreadyFailing": out["checksAlreadyFailing"],
        "skipped": out["skipped"], "costUsd": out["costUsd"], "spend": out["spend"],
        "callsWithUnknownCost": out["callsWithUnknownCost"],
        "approve": format!("wg approve --repo {repo} --pr {pr} --by \"$(git config user.name)\""),
    }))
}

/// The owner's approval of a shift PR: HEAD must be the PR head, then the finalize chain runs on
/// the run's kernel tokens; on pass the branch is pushed, the PR marked ready and the issue
/// labelled `wg:verified`.
pub fn approve(
    dirs: &Dirs,
    cfg: &Config,
    gh: &Gh,
    repo: &str,
    pr: u64,
    by: &str,
) -> Result<Value, String> {
    repo_cfg(cfg, repo)?;
    let rec = record::by_pr(dirs, repo, pr)?;
    let worktree = Path::new(&rec.worktree);
    let view = gh.pr_view(repo, pr)?;
    if view.state != "OPEN" {
        return Err(format!("approve refused: PR #{pr} is {}", view.state));
    }
    let head = crate::git::head(worktree)?;
    if head != view.head_oid {
        return Err(format!(
            "approve refused: the worktree HEAD {head} is not the PR head {}",
            view.head_oid
        ));
    }
    let run = rec.runs.last().ok_or("the task has no run")?;
    let mut out = crate::cli::finalize::finalize_at(
        dirs,
        cfg,
        worktree,
        run,
        by,
        &format!("chore: approve #{} after the build", rec.issue),
        "chore: record Haruspex verification",
    )?;
    out["pr"] = json!(pr);
    if out["passed"] != true {
        return Ok(out);
    }
    crate::git::push_task_branch(worktree, &rec.branch)?;
    gh.pr_ready(repo, pr)?;
    intake::relabel(dirs, gh, repo, rec.issue, &[VERIFIED], &[PR_OPEN])?;
    record::save(
        dirs,
        &TaskRecord {
            status: "verified".into(),
            updated_at: crate::journal::now(),
            ..rec
        },
    )?;
    out["ready"] = json!(true);
    Ok(out)
}
