//! The release line (design §7.3 step 10, §7.4, Phase 4): for a repo with `release = true`, each
//! task the owner verified (`wg approve` passed) whose PR has merged runs `release.toml` in a
//! worktree recreated from the merge commit. It re-runs Haruspex verify there with the pinned
//! commands, then writes notes and a plan, rehearses in the sim sandbox, evaluates
//! ready-to-release and stops at the owner's human stage. It never records a deployment, never
//! deploys and never touches a server: its only execution off the Mac is the sim sandbox.
//!
//! Outcomes: `release-ready` only on the kernel's evidence; an incident labels the issue
//! `wg:incident` and stops the shift (§7.3 step 9); a refusal or a failed stage is `release-stuck`
//! until the owner runs `wg shift retry-release`; any other error leaves the task `verified`, so
//! the next shift tries again. Every one is in the shift report.

use super::config::RepoCfg;
use super::intake;
use super::owner::VERIFIED;
use super::record::{self, TaskRecord};
use super::runctx;
use super::task::{incident_class, stop_reason, tainted_earlier, Env};
use crate::engine::driver::{self, End, Ran, Start};
use crate::engine::kernel::Kernel;
use crate::haruspex::reverify::REFUSED;
use crate::journal::now;
use crate::memory_store::Loaded;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use wg_factory::api::Memory;

pub const RELEASE_READY: &str = "wg:release-ready";
const INCIDENT: &str = "wg:incident";

/// Verified tasks of `repo` with a PR, oldest claim first.
pub fn candidates(e: &Env, repo: &str) -> Result<Vec<TaskRecord>, String> {
    Ok(record::all(e.dirs)?
        .into_iter()
        .filter(|r| r.repo == repo && r.status == "verified" && r.pr.is_some())
        .collect())
}

/// Runs the release line for `repo`; `Some(why)` when the shift must stop (an incident).
pub fn line(e: &Env, repo: &RepoCfg) -> Result<Option<String>, String> {
    if !repo.release {
        return Ok(None);
    }
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
        let issue = rec.issue;
        let result = match head_refusal(&rec, &view.head_oid) {
            Some(why) => Err(why),
            None => prepare(e, repo, rec.clone(), &merge),
        };
        let (status, label, comment, run, note) = match &result {
            Ok((End::AwaitingOwner { .. }, out)) => (
                "release-ready",
                Some(RELEASE_READY),
                format!(
                    "workgraph prepared the release of #{pr} at merge commit {merge}: notes, plan, a rehearsal in the sim sandbox, and ready-to-release has only the owner's approval left. It stops here; the factory never records a deployment and never deploys.\n\n{}",
                    fence(&notes(out))
                ),
                out["runId"].as_str().map(str::to_string),
                None,
            ),
            // The sandbox was unavailable or the shift stopped: try again next shift.
            Ok((End::Parked(why), out)) => {
                e.journal.append(
                    "release_parked",
                    json!({ "repo": repo.name, "issue": issue, "why": why, "run": out["runId"] }),
                )?;
                remove_worktree(e, repo, issue)?;
                continue;
            }
            // The shift stops; GitHub learns the class only (§7.3 step 8).
            Ok((End::Incident(why), out)) => {
                e.journal.append(
                    "incident",
                    json!({ "at": "release", "repo": repo.name, "issue": issue, "run": out["runId"], "why": why }),
                )?;
                (
                    "incident",
                    Some(INCIDENT),
                    format!("An incident aborted the release preparation of #{pr} (class: {}). The shift stopped; details are in the owner's journal.", incident_class(why)),
                    out["runId"].as_str().map(str::to_string),
                    Some(format!("incident: {}", incident_class(why))),
                )
            }
            Ok((_, out)) => {
                let note = out["note"].as_str().unwrap_or("the release line stopped");
                stuck(pr, note, out["runId"].as_str().map(str::to_string))
            }
            Err(why) if why.starts_with(REFUSED) => stuck(pr, why, None),
            // A worktree, gh or Haruspex error: the task stays verified for the next shift.
            Err(why) => {
                e.journal.append(
                    "release_error",
                    json!({ "repo": repo.name, "issue": issue, "pr": pr, "merge": merge, "why": why }),
                )?;
                continue;
            }
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
            json!({ "repo": repo.name, "issue": issue, "pr": pr, "merge": merge, "outcome": status, "run": run, "note": note }),
        )?;
        // A stuck release or an incident keeps its worktree for the owner's diagnosis.
        if status == "release-ready" {
            remove_worktree(e, repo, issue)?;
        }
        if status == "incident" {
            return Ok(Some("an incident in the release line".into()));
        }
    }
    Ok(None)
}

type Outcome = (
    &'static str,
    Option<&'static str>,
    String,
    Option<String>,
    Option<String>,
);

fn stuck(pr: u64, note: &str, run: Option<String>) -> Outcome {
    (
        "release-stuck",
        None,
        format!(
            "workgraph could not prepare the release of #{pr}:\n\n{}",
            fence(note)
        ),
        run,
        Some(crate::ids::one_line(note, 500)),
    )
}

/// Where `issue`'s release worktree lives, relative to the worktrees root.
fn worktree_rel(repo: &str, issue: u64) -> String {
    format!("{}/release-{issue}", record::slug(repo))
}

/// Removes `issue`'s release worktree through the path-guarded git helper; a failure is journaled
/// and never fails the release line.
fn remove_worktree(e: &Env, repo: &RepoCfg, issue: u64) -> Result<(), String> {
    let root = e.dirs.worktrees();
    let path = root.join(worktree_rel(&repo.name, issue));
    if !path.exists() {
        return Ok(());
    }
    match crate::git::remove_worktree(Path::new(&repo.path), &root, &path) {
        Ok(()) => e.journal.append(
            "release_worktree_removed",
            json!({ "repo": repo.name, "issue": issue, "path": path }),
        ),
        Err(why) => e.journal.append(
            "warning",
            json!({ "warning": format!("release worktree {}: {why}", path.display()) }),
        ),
    }
    .map(|_| ())
}

/// The PR merged with the head the owner verified, or the release is refused: a commit pushed to
/// the task branch after `wg approve` was never reviewed.
fn head_refusal(rec: &TaskRecord, head: &str) -> Option<String> {
    match rec.approved_head.as_deref() {
        Some(h) if h == head => None,
        Some(h) => Some(format!(
            "{REFUSED}the PR merged at head {head}, not at {h}, the commit the owner verified: what was pushed after `wg approve` was never reviewed"
        )),
        None => Some(format!(
            "{REFUSED}the task record names no commit the owner verified"
        )),
    }
}

/// What the kernel holds against calling the release ready, whatever the factory said: a plan
/// the writer marked ready, and a rehearsal of the configured steps where every step exited 0.
pub fn unproven(k: &Kernel) -> Option<String> {
    if k.release_plan_ready != Some(true) {
        return Some("the kernel holds no release plan marked ready".into());
    }
    let rehearsed = k.sandbox_runs.iter().any(|r| {
        !r.steps().is_empty()
            && r.steps()
                .iter()
                .all(|(n, e)| n.starts_with("release-rehearsal-") && *e == 0)
    });
    (!rehearsed).then(|| "the kernel holds no passing rehearsal from the sandbox".into())
}

/// `wg shift retry-release`: the owner puts a stuck release back on the line for the next shift.
pub fn retry(
    dirs: &Dirs,
    cfg: &crate::config::Config,
    repo: &str,
    issue: u64,
) -> Result<Value, String> {
    cfg.repo(repo)
        .ok_or_else(|| format!("{repo} is not a [[repo]] in factory.toml"))?;
    let rec = record::load(dirs, repo, issue)?
        .ok_or_else(|| format!("{repo}#{issue} has no task record"))?;
    if rec.status != "release-stuck" {
        return Err(format!(
            "{repo}#{issue} is {}, not release-stuck",
            rec.status
        ));
    }
    record::save(
        dirs,
        &TaskRecord {
            status: "verified".into(),
            updated_at: now(),
            ..rec
        },
    )?;
    Ok(json!({ "repo": repo, "issue": issue, "status": "verified" }))
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
        &worktree_rel(&repo.name, rec.issue),
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
            // The title is issue text: fenced as dev fences the issue (§14 Z4).
            task: format!(
                "Prepare the release of {}#{} (Haruspex task {task}), merged as {merge}. The issue it came from:\n\n{}",
                repo.name,
                rec.issue,
                crate::untrusted::issue::fenced_title(rec.issue, &crate::ids::one_line(&rec.title, 200))
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
        let ran = driver::run_task(ctx, start, &wg_factory::engine::step)?;
        let ran = match (&ran.end, unproven(&ran.kernel)) {
            (End::AwaitingOwner { .. }, Some(why)) => {
                ctx.journal.append(
                    "warning",
                    json!({ "refused": format!("release-ready: {why}") }),
                )?;
                Ran {
                    end: End::Failed(format!("refused: {why}")),
                    ..ran
                }
            }
            _ => ran,
        };
        runctx::finish(ctx, ran)
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
        return Err(format!("{REFUSED}task {task} declares no required_checks"));
    }
    let mut pins = BTreeMap::new();
    for name in &required {
        let cmd = if tainted {
            repo.sandbox
                .as_ref()
                .and_then(|s| s.verify.get(name))
                .map(|c| c.replace("{task}", task))
                .ok_or_else(|| format!("{REFUSED}host verify refused for tainted task; add [repo.sandbox].verify or verify yourself"))?
        } else {
            e.cfg
                .check(&repo.name, name)
                .map(|c| c.project.clone())
                .ok_or_else(|| {
                    format!(
                        "{REFUSED}check {name} has no approved command for {}",
                        repo.name
                    )
                })?
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
        task,
        &pins,
        sandbox_project.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::SandboxRun;

    fn rehearsal(exits: &[i64]) -> SandboxRun {
        let steps = exits
            .iter()
            .enumerate()
            .map(|(i, e)| (format!("release-rehearsal-{}", i + 1), *e))
            .collect();
        SandboxRun::restore("j-1", "n", "rehearse", "t", "img", steps, true)
    }

    #[test]
    fn release_ready_needs_the_kernels_plan_and_a_passing_rehearsal() {
        let k = Kernel::default();
        assert!(unproven(&k).unwrap().contains("plan"));
        let k = Kernel {
            release_plan_ready: Some(false),
            ..k
        };
        assert!(unproven(&k).unwrap().contains("plan"));
        let k = Kernel {
            release_plan_ready: Some(true),
            ..k
        };
        assert!(unproven(&k).unwrap().contains("rehearsal"));
        let k = Kernel {
            sandbox_runs: vec![rehearsal(&[0, 1])],
            ..k
        };
        assert!(unproven(&k).unwrap().contains("rehearsal"));
        let k = Kernel {
            sandbox_runs: vec![rehearsal(&[0, 0])],
            ..k
        };
        assert_eq!(unproven(&k), None);
    }

    #[test]
    fn only_the_head_the_owner_verified_is_released() {
        let rec = TaskRecord {
            approved_head: Some("abc".into()),
            ..TaskRecord::default()
        };
        assert_eq!(head_refusal(&rec, "abc"), None);
        let why = head_refusal(&rec, "def").unwrap();
        assert!(
            why.starts_with(REFUSED) && why.contains("never reviewed"),
            "{why}"
        );
        let none = head_refusal(&TaskRecord::default(), "abc").unwrap();
        assert!(none.starts_with(REFUSED), "{none}");
    }
}
