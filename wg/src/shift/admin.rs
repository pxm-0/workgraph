//! Shift maintenance (design §7.4): `resume` reruns a shift's interrupted tasks, replaying what
//! their last run already got; `recover` requeues stale claims; `gc` removes finished worktrees;
//! `status` says what is going on.

use super::intake::{self, IN_SHIFT};
use super::lock::{self, Taken};
use super::record::{self, TaskRecord};
use super::run::{finished, work_one};
use super::task::{Done, Env, Kind};
use crate::config::Config;
use crate::gh::Gh;
use crate::journal::{now, Journal};
use crate::state_dirs::Dirs;
use crate::untrusted::issue;
use serde_json::{json, Value};
use std::path::Path;

const DAY: u64 = 24 * 3600;

/// `(repo, issue)` of tasks the shift claimed but never finished, or parked as resumable.
pub fn interrupted(events: &[Value]) -> Vec<(String, u64)> {
    let key = |e: &Value| {
        (
            e["repo"].as_str().unwrap_or("").to_string(),
            e["issue"].as_u64().unwrap_or(0),
        )
    };
    let mut open: Vec<(String, u64)> = Vec::new();
    for e in events {
        match e["kind"].as_str() {
            Some("task_claimed") if !open.contains(&key(e)) => open.push(key(e)),
            Some("task_finished") if e["resumable"] != true => open.retain(|k| *k != key(e)),
            _ => {}
        }
    }
    open
}

/// The installed binary a journal names, when it differs from this one and still exists.
pub fn pinned_binary(dirs: &Dirs, rec: &TaskRecord) -> Option<std::path::PathBuf> {
    let run = rec.runs.last()?;
    let journal = crate::journal::read(&dirs.run(run).join("journal.jsonl")).ok()?;
    let (binary, _) = crate::claude::replay::versions(&journal)?;
    let exe = dirs
        .state()
        .join("bin")
        .join(format!("wg-{binary}"))
        .join("wg");
    (binary != crate::claude::replay::binary_hash() && exe.exists()).then_some(exe)
}

/// Reruns the interrupted tasks of shift `id`. A task whose run came from another binary that is
/// still installed re-execs that binary; otherwise a version mismatch reruns from scratch.
pub fn resume(dirs: &Dirs, cfg: &Config, id: &str) -> Result<Value, String> {
    crate::ids::run_id(id).map_err(|_| format!("{id:?} is not a shift id"))?;
    let dir = dirs.shifts().join(id);
    let events = crate::journal::read(&dir.join("journal.jsonl"))?;
    let todo = interrupted(&events);
    if std::env::var_os("WG_RESUME_REEXEC").is_none() {
        for (repo, n) in &todo {
            if let Some(exe) = record::load(dirs, repo, *n)?.and_then(|r| pinned_binary(dirs, &r)) {
                use std::os::unix::process::CommandExt;
                let err = std::process::Command::new(&exe)
                    .args(["shift", "resume", id])
                    .env("WG_RESUME_REEXEC", "1")
                    .exec();
                return Err(format!("cannot re-exec {}: {err}", exe.display()));
            }
        }
    }
    let _lock = match lock::take(&dirs.shift_lock())? {
        Taken::Held(pid) => {
            return Ok(json!({ "skipped": format!("a shift (pid {pid}) holds the lock") }))
        }
        Taken::Fresh(l) | Taken::Stale(l, _) => l,
    };
    let j = Journal::open(&dir.join("journal.jsonl"), id)?;
    let gh = Gh::new(&cfg.gh_bin, dir.join("gh"));
    // A resume is the owner's explicit act, so the shift window does not apply; STOP still does.
    let env = Env {
        dirs,
        cfg,
        gh: &gh,
        shift_id: id,
        journal: &j,
        deadline: u64::MAX,
        sandbox: Default::default(),
    };
    let mut resumed = Vec::new();
    for (repo, n) in todo {
        if dirs.stop_file().exists() {
            j.append("stopped", json!({ "why": "the STOP file exists" }))?;
            break;
        }
        let Some(rc) = cfg.repo(&repo) else {
            j.append(
                "error",
                json!({ "repo": repo, "issue": n, "error": "no longer a [[repo]]" }),
            )?;
            continue;
        };
        let i = gh.issue(&repo, n)?;
        let rec = record::load(dirs, &repo, n)?;
        if rec
            .as_ref()
            .is_some_and(|r| r.body_sha256 != issue::body_sha256(&i))
        {
            // A self issue goes back to the owner's wg:self queue, never the dev queue.
            let queue = super::selflane::lane_shift(cfg, rc).label;
            intake::relabel(dirs, &gh, &repo, n, &[&queue], &[IN_SHIFT])?;
            gh.comment(
                &repo,
                n,
                "workgraph parked this task: the issue changed after the shift claimed it.",
            )?;
            let d = Done {
                kind: Kind::Parked,
                run: None,
                reason: "the issue changed after the claim".into(),
                pr_url: None,
                rate_limited: false,
                escalations: json!([]),
            };
            finished(&j, &repo, &i, &d)?;
            resumed.push(json!({ "repo": repo, "issue": n, "outcome": "parked" }));
            continue;
        }
        let d = work_one(&env, rc, &i, true)?;
        super::selflane::authored(&env, rc, n, &d)?;
        resumed.push(json!({ "repo": repo, "issue": n, "outcome": d.kind.name(), "run": d.run }));
        if matches!(d.kind, Kind::Incident) {
            break;
        }
    }
    let report = super::report::write(dirs, id)?;
    let reexecuted = std::env::var_os("WG_RESUME_REEXEC").is_some();
    Ok(
        json!({ "shift": id, "resumed": resumed, "report": report["report"], "reexecuted": reexecuted }),
    )
}

/// Claims older than 24 hours with no PR and no live shift go back to the queue, but only an issue
/// whose one state label is still `wg:in-shift`: any other means someone already decided its fate.
pub fn recover(dirs: &Dirs, cfg: &Config, gh: &Gh) -> Result<Value, String> {
    if let Some(pid) = lock::holder(&dirs.shift_lock()).filter(|p| lock::alive(*p)) {
        return Ok(json!({ "skipped": format!("a shift (pid {pid}) is running") }));
    }
    let mut out = Vec::new();
    for rec in record::all(dirs)? {
        if rec.status != "in-shift" || rec.updated_at + DAY > now() {
            continue;
        }
        if gh.pr_for_head(&rec.repo, &rec.branch)?.is_some() {
            continue;
        }
        let labels = gh.issue(&rec.repo, rec.issue)?.labels;
        let states: Vec<&str> = labels
            .iter()
            .map(String::as_str)
            .filter(|l| l.starts_with("wg:") && *l != intake::P1)
            .collect();
        if states != [IN_SHIFT] {
            continue;
        }
        let queue = cfg.repo(&rec.repo).map_or(cfg.shift.label.clone(), |rc| {
            super::selflane::lane_shift(cfg, rc).label
        });
        intake::relabel(dirs, gh, &rec.repo, rec.issue, &[&queue], &[IN_SHIFT])?;
        gh.comment(
            &rec.repo,
            rec.issue,
            "workgraph recovered this claim: its shift ended without a result. It is queued again.",
        )?;
        out.push(json!({ "repo": rec.repo, "issue": rec.issue }));
        record::save(
            dirs,
            &TaskRecord {
                status: "parked".into(),
                updated_at: now(),
                ..rec
            },
        )?;
    }
    Ok(json!({ "recovered": out }))
}

/// Removes worktrees whose PR merged or closed, and stuck ones older than 14 days. A verified
/// task of a repo with a release line keeps its status, so the line still takes it. Run tmp dirs
/// a crashed run left are swept first, so a `gh` failure does not stop that.
pub fn gc(dirs: &Dirs, cfg: &Config, gh: &Gh) -> Result<Value, String> {
    let tmp = crate::scratch::sweep_tmp(dirs);
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for rec in record::all(dirs)? {
        let path = Path::new(&rec.worktree);
        if rec.status == "gone" || !path.exists() {
            continue;
        }
        let done = match rec.pr {
            Some(pr) => ["MERGED", "CLOSED"].contains(&gh.pr_view(&rec.repo, pr)?.state.as_str()),
            None => rec.status == "stuck" && rec.updated_at + 14 * DAY <= now(),
        };
        let Some(repo) = cfg.repo(&rec.repo).filter(|_| done) else {
            kept.push(json!({ "repo": rec.repo, "issue": rec.issue, "status": rec.status }));
            continue;
        };
        crate::git::remove_worktree(Path::new(&repo.path), &dirs.worktrees(), path)?;
        removed.push(json!({ "repo": rec.repo, "issue": rec.issue, "worktree": rec.worktree }));
        // A verified task stays one for the release line, which recreates its own worktree; a
        // stuck release stays one for `wg shift retry-release`.
        let awaits_release =
            repo.release && ["verified", "release-stuck"].contains(&rec.status.as_str());
        record::save(
            dirs,
            &TaskRecord {
                status: if awaits_release {
                    rec.status.clone()
                } else {
                    "gone".into()
                },
                updated_at: now(),
                ..rec
            },
        )?;
    }
    Ok(json!({ "removed": removed, "kept": kept, "tmp": tmp }))
}

pub fn status(dirs: &Dirs) -> Result<Value, String> {
    let holder = lock::holder(&dirs.shift_lock());
    let mut by: std::collections::BTreeMap<String, u64> = Default::default();
    for r in record::all(dirs)? {
        *by.entry(r.status).or_default() += 1;
    }
    let latest = super::report::latest(dirs)?;
    let report = latest
        .as_ref()
        .and_then(|id| std::fs::read_to_string(dirs.shifts().join(id).join("report.json")).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    Ok(json!({
        "running": holder.filter(|p| lock::alive(*p)),
        "stop": dirs.stop_file().exists(),
        "latest": latest,
        "stopped": report.as_ref().map(|r| r["stopped"].clone()),
        "tasksByStatus": by,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_tasks_are_claimed_and_not_finished() {
        let e = |kind: &str, n: u64| json!({ "kind": kind, "repo": "o/r", "issue": n });
        let mut parked = e("task_finished", 4);
        parked["resumable"] = json!(true);
        let events = [
            e("task_claimed", 1),
            e("task_finished", 1),
            e("task_claimed", 2),
            e("task_claimed", 3),
            e("task_finished", 3),
            e("task_claimed", 3),
            e("task_claimed", 4),
            parked,
        ];
        assert_eq!(
            interrupted(&events),
            vec![
                ("o/r".to_string(), 2),
                ("o/r".to_string(), 3),
                ("o/r".to_string(), 4)
            ]
        );
    }
}
