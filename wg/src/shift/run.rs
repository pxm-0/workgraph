//! `wg shift run` (design §7.3): lock, window, preflight, intake per repo, one task at a time with
//! the stop conditions checked between tasks (and, inside a task, between steps), then the report.

use super::config::RepoCfg;
use super::intake;
use super::lock::{self, Taken};
use super::task::{self, Done, Env, Kind};
use crate::config::Config;
use crate::gh::Gh;
use crate::journal::{now, Journal};
use crate::state_dirs::{private_dir, Dirs};
use crate::untrusted::issue::Issue;
use serde_json::{json, Value};
use std::path::Path;

/// Local wall-clock text from `date`, the one source of local time (no time zone crate).
pub fn local(format: &str) -> Result<String, String> {
    let out = std::process::Command::new("/bin/date")
        .arg(format!("+{format}"))
        .output()
        .map_err(|e| format!("date failed to start: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Minutes left in the window `[start, until)` at local minute `now`, wrapping past midnight;
/// `None` when the window is closed. A window with `start == until` is always closed.
pub fn minutes_left(start: u32, until: u32, now: u32) -> Option<u32> {
    let since = (now + 1440 - start) % 1440;
    let length = (until + 1440 - start) % 1440;
    (since < length).then(|| length - since)
}

/// A shift id from the local clock, `YYYYMMDD-HHMM`, made unique under `shifts/`.
fn shift_id(dirs: &Dirs) -> Result<String, String> {
    let base = local("%Y%m%d-%H%M")?;
    let mut id = base.clone();
    let mut n = 1;
    while dirs.shifts().join(&id).exists() {
        n += 1;
        id = format!("{base}-{n}");
    }
    Ok(id)
}

/// Global preflight (design §7.3 step 3); any failure stops the shift before intake.
fn preflight(dirs: &Dirs, cfg: &Config, gh: &Gh) -> Result<(), String> {
    crate::pipelines::load(crate::assets::DEV_PIPELINE, cfg.max_calls_per_task)?;
    if let Some(p) = crate::cli::doctor::user_settings_problem(&dirs.claude_settings())? {
        return Err(p);
    }
    let pin = cfg.haruspex.as_ref().ok_or("no Haruspex CLI is pinned")?;
    crate::haruspex::cli::check_location(&pin.cli, &[dirs.work(), dirs.state()])?;
    if crate::haruspex::cli::sha256_file(Path::new(&pin.cli))? != pin.sha256 {
        return Err("the Haruspex CLI no longer matches its pin".into());
    }
    gh.auth_status()?;
    for id in &cfg.auth.fallback {
        let Some(p) = cfg.auth.get(id) else { continue };
        if let Some(svc) = &p.keychain_service {
            let found = std::process::Command::new(&cfg.security_bin)
                .args(["find-generic-password", "-s", svc])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !found {
                return Err(format!("provider {id}: keychain item {svc} is missing"));
            }
        }
    }
    crate::cli::live::probe_all(cfg, dirs).map(|_| ())
}

/// Per-repo preflight: fetchable, the dev stack exists on the default branch, the compose
/// override files exist, and no unallowlisted project agent config on the default branch.
fn repo_problem(repo: &RepoCfg) -> Option<String> {
    let path = Path::new(&repo.path);
    if let Err(e) = crate::git::fetch(path) {
        return Some(format!("cannot fetch: {e}"));
    }
    let rev = format!("origin/{}", repo.default_branch);
    for need in ["compose.dev.yml", ".devcontainer"] {
        if !crate::git::exists_at(path, &rev, need) {
            return Some(format!("RepoNotCompliant: {rev} has no {need}"));
        }
    }
    let files = repo
        .factory_compose
        .as_deref()
        .unwrap_or("")
        .split_whitespace();
    if let Some(f) = files
        .filter(|t| t.starts_with('/'))
        .find(|t| !Path::new(t).exists())
    {
        return Some(format!("the factory compose override {f} does not exist"));
    }
    match task::config_problem(repo, |rel| crate::git::blob_sha256(path, &rev, rel)) {
        Ok(p) => p.map(|p| format!("{p} on {rev}")),
        Err(e) => Some(format!("cannot read {rev}: {e}")),
    }
}

pub struct Counters {
    pub tasks: u32,
    pub failures_in_a_row: u32,
}

/// Why intake stops before the next task, if it must.
pub fn stop_before_next(
    cfg: &Config,
    c: &Counters,
    stop_file: &Path,
    deadline: u64,
) -> Option<String> {
    if let Some(why) = task::stop_reason(stop_file, deadline) {
        return Some(why);
    }
    if c.tasks >= cfg.shift.max_tasks {
        return Some(format!("max_tasks ({}) reached", cfg.shift.max_tasks));
    }
    (c.failures_in_a_row >= cfg.shift.max_consecutive_failures)
        .then(|| format!("{} consecutive failures", c.failures_in_a_row))
}

/// Why the shift must stop after this task's outcome.
pub fn stop_after(d: &Done) -> Option<String> {
    match d.kind {
        Kind::Incident => Some("an incident".into()),
        Kind::Parked if d.rate_limited => Some("rate limited with no usable provider left".into()),
        Kind::Parked if d.reason.starts_with("shift stopped") => Some(d.reason.clone()),
        _ => None,
    }
}

/// Records one task's outcome in the shift journal. A resumable park stays open for resume.
pub fn finished(j: &Journal, repo: &str, i: &Issue, d: &Done) -> Result<(), String> {
    j.append(
        "task_finished",
        json!({ "repo": repo, "issue": i.number, "title": i.title, "run": d.run, "outcome": d.kind.name(),
                "reason": d.reason, "pr": d.pr_url, "escalations": d.escalations, "resumable": d.resumable() }),
    )
    .map(|_| ())
}

/// Marks the task record stuck after a kernel error, so `recover` never requeues it.
fn record_stuck(e: &Env, repo: &str, n: u64) -> Result<(), String> {
    match super::record::load(e.dirs, repo, n)? {
        Some(rec) => super::record::save(
            e.dirs,
            &super::record::TaskRecord {
                status: "stuck".into(),
                updated_at: now(),
                ..rec
            },
        ),
        None => Ok(()),
    }
}

/// Works `issue`, turning a kernel error into a stuck outcome so the issue never stays claimed.
pub fn work_one(e: &Env, repo: &RepoCfg, i: &Issue, resume: bool) -> Result<Done, String> {
    let d = match task::work(e, repo, i, resume) {
        Ok(d) => d,
        Err(why) => {
            e.journal.append(
                "error",
                json!({ "repo": repo.name, "issue": i.number, "error": why }),
            )?;
            let d = Done {
                kind: Kind::Stuck,
                run: None,
                reason: format!("the task failed: {why}"),
                pr_url: None,
                rate_limited: false,
                escalations: json!([]),
            };
            if let Err(e2) = record_stuck(e, &repo.name, i.number) {
                e.journal
                    .append("error", json!({ "issue": i.number, "error": e2 }))?;
            }
            // Best effort: the claim stays visible as wg:in-shift if GitHub is down too.
            if let Err(e2) = intake::relabel(
                e.dirs,
                e.gh,
                &repo.name,
                i.number,
                &["wg:stuck"],
                &[intake::IN_SHIFT],
            )
            .and_then(|_| {
                e.gh.comment(
                    &repo.name,
                    i.number,
                    &format!(
                        "workgraph stuck:\n\n```text\n{}\n```",
                        d.reason.replace("```", "'''")
                    ),
                )
            }) {
                e.journal
                    .append("error", json!({ "issue": i.number, "error": e2 }))?;
            }
            d
        }
    };
    finished(e.journal, &repo.name, i, &d)?;
    Ok(d)
}

pub fn run(dirs: &Dirs, cfg: &Config) -> Result<Value, String> {
    if dirs.stop_file().exists() {
        return Ok(json!({ "skipped": "the STOP file exists" }));
    }
    let lock = match lock::take(&dirs.shift_lock())? {
        Taken::Held(pid) => {
            return Ok(json!({ "skipped": format!("another shift (pid {pid}) holds the lock") }))
        }
        Taken::Fresh(l) => (l, None),
        Taken::Stale(l, pid) => (l, Some(pid)),
    };
    let id = shift_id(dirs)?;
    let dir = dirs.shifts().join(&id);
    private_dir(&dir)?;
    let j = Journal::open(&dir.join("journal.jsonl"), &id)?;
    j.append(
        "shift_started",
        json!({ "binary": crate::claude::replay::binary_hash(), "stale_lock_pid": lock.1 }),
    )?;
    let now_min = super::config::minutes(&local("%H:%M")?)?;
    let Some(left) = minutes_left(cfg.shift.start, cfg.shift.until, now_min) else {
        j.append("stopped", json!({ "why": "window closed" }))?;
        return super::report::write(dirs, &id);
    };
    let deadline = now() + u64::from(left) * 60;
    let gh = Gh::new(&cfg.gh_bin, dir.join("gh"));
    if let Err(why) = preflight(dirs, cfg, &gh) {
        j.append("stopped", json!({ "why": format!("preflight: {why}") }))?;
        return super::report::write(dirs, &id);
    }
    j.append("preflight", json!({ "ok": true }))?;
    let sandbox = super::sandbox::start(cfg, &j)?;
    let env = Env {
        dirs,
        cfg,
        gh: &gh,
        shift_id: &id,
        journal: &j,
        deadline,
        sandbox: sandbox.status.clone(),
    };
    let mut c = Counters {
        tasks: 0,
        failures_in_a_row: 0,
    };
    'repos: for repo in &cfg.repos {
        if let Some(why) = repo_problem(repo) {
            j.append("repo_skipped", json!({ "repo": repo.name, "why": why }))?;
            continue;
        }
        let picked = match super::record::issues_of(dirs, &repo.name).and_then(|reruns| {
            intake::intake(
                &gh,
                &cfg.shift,
                &repo.name,
                &intake::created(dirs, &repo.name),
                &reruns,
            )
        }) {
            Ok(p) => p,
            Err(why) => {
                j.append(
                    "repo_skipped",
                    json!({ "repo": repo.name, "why": format!("intake: {why}") }),
                )?;
                continue;
            }
        };
        for (n, why) in &picked.skipped {
            j.append(
                "issue_skipped",
                json!({ "repo": repo.name, "issue": n, "why": why }),
            )?;
        }
        for i in &picked.picked {
            if let Some(why) = stop_before_next(cfg, &c, &dirs.stop_file(), deadline) {
                j.append("stopped", json!({ "why": why }))?;
                break 'repos;
            }
            // Intake ran maybe hours ago: the owner may have closed, un-queued or edited it since.
            if let Some(why) = intake::still_queued(&gh, &cfg.shift, &repo.name, i) {
                j.append(
                    "issue_skipped",
                    json!({ "repo": repo.name, "issue": i.number, "why": why }),
                )?;
                continue;
            }
            let d = work_one(&env, repo, i, false)?;
            c.tasks += 1;
            c.failures_in_a_row = match d.kind {
                Kind::Stuck | Kind::Incident => c.failures_in_a_row + 1,
                _ => 0,
            };
            if let Some(why) = stop_after(&d) {
                j.append("stopped", json!({ "why": why }))?;
                break 'repos;
            }
        }
    }
    super::sandbox::end(cfg, &j, &sandbox)?;
    j.append("shift_finished", json!({ "tasks": c.tasks }))?;
    let out = super::report::write(dirs, &id);
    drop(lock);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_wraps_past_midnight_and_closes_after_until() {
        assert_eq!(minutes_left(90, 420, 90), Some(330));
        assert_eq!(minutes_left(90, 420, 419), Some(1));
        assert_eq!(minutes_left(90, 420, 420), None);
        assert_eq!(
            minutes_left(90, 420, 14 * 60),
            None,
            "a manual 14:00 run after a 01:30-07:00 window"
        );
        assert_eq!(minutes_left(23 * 60, 60, 30), Some(30));
        assert_eq!(minutes_left(0, 0, 0), None);
        assert!(local("%H:%M").unwrap().len() == 5);
    }

    #[test]
    fn stop_conditions() {
        let cfg =
            crate::config::parse("[shift]\nmax_tasks = 2\nmax_consecutive_failures = 2\n").unwrap();
        let none = Path::new("/nonexistent/STOP");
        let later = now() + 600;
        assert_eq!(
            stop_before_next(
                &cfg,
                &Counters {
                    tasks: 1,
                    failures_in_a_row: 1
                },
                none,
                later
            ),
            None
        );
        assert!(stop_before_next(
            &cfg,
            &Counters {
                tasks: 2,
                failures_in_a_row: 0
            },
            none,
            later
        )
        .unwrap()
        .contains("max_tasks"));
        assert!(stop_before_next(
            &cfg,
            &Counters {
                tasks: 1,
                failures_in_a_row: 2
            },
            none,
            later
        )
        .unwrap()
        .contains("consecutive"));
        let d = |kind, reason: &str, rate_limited| Done {
            kind,
            run: None,
            reason: reason.into(),
            pr_url: None,
            rate_limited,
            escalations: json!([]),
        };
        assert!(stop_after(&d(Kind::Incident, "x", false)).is_some());
        assert!(stop_after(&d(Kind::Parked, "rate limited", true))
            .unwrap()
            .contains("rate"));
        assert!(stop_after(&d(
            Kind::Parked,
            "shift stopped: the STOP file exists",
            false
        ))
        .is_some());
        assert_eq!(stop_after(&d(Kind::Parked, "call cap", false)), None);
        assert_eq!(stop_after(&d(Kind::Stuck, "x", false)), None);
    }
}
