//! One shift task (design §7.3 steps 5-8, §7.4): claim, the worktree, the install step, a run of
//! the dev chain with publish, and the outcome written back to the issue and the task record.

use super::config::{RepoCfg, PROJECT_CONFIG};
use super::intake::{self, IN_SHIFT};
use super::record::{self, TaskRecord};
use super::runctx;
use crate::claude::replay::Replay;
use crate::config::Config;
use crate::engine::driver::{self, End, Start};
use crate::gh::Gh;
use crate::journal::{now, Journal};
use crate::state_dirs::Dirs;
use crate::untrusted::issue::{self, Issue};
use crate::{git, memory_store, tokens_store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const INSTALL_TIMEOUT: Duration = Duration::from_secs(1800);

/// What a shift needs to work its tasks.
pub struct Env<'a> {
    pub dirs: &'a Dirs,
    pub cfg: &'a Config,
    pub gh: &'a Gh,
    pub shift_id: &'a str,
    pub journal: &'a Journal,
    /// Unix seconds after which no new step starts (`[shift].until`).
    pub deadline: u64,
    /// The sandbox doctor's verdict for this shift, shared by its tasks (design §7.3 step 3).
    pub sandbox: crate::sandbox::client::Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pr,
    NeedsDecision,
    Stuck,
    Parked,
    Incident,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Pr => "pr-open",
            Kind::NeedsDecision => "needs-decision",
            Kind::Stuck => "stuck",
            Kind::Parked => "parked",
            Kind::Incident => "incident",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Done {
    pub kind: Kind,
    pub run: Option<String>,
    pub reason: String,
    pub pr_url: Option<String>,
    /// No provider left: the shift stops intake.
    pub rate_limited: bool,
    pub escalations: Value,
}

impl Done {
    /// Parked because the shift stopped mid-task: the task stays claimed and `wg shift resume`
    /// replays what its run already got (design §7.3 step 9).
    pub fn resumable(&self) -> bool {
        self.kind == Kind::Parked && self.reason.starts_with("shift stopped")
    }
}

fn done(kind: Kind, run: Option<&str>, reason: impl Into<String>) -> Done {
    Done {
        kind,
        run: run.map(str::to_string),
        reason: reason.into(),
        pr_url: None,
        rate_limited: false,
        escalations: json!([]),
    }
}

/// Why the shift must stop now: the STOP file or the end of the window.
pub fn stop_reason(stop_file: &Path, deadline: u64) -> Option<String> {
    if stop_file.exists() {
        return Some("the STOP file exists".into());
    }
    (now() >= deadline).then(|| "the shift window closed".into())
}

/// Project agent config the owner did not allowlist (design §5.1), given each file's sha256.
pub fn config_problem(
    repo: &RepoCfg,
    sha_of: impl Fn(&str) -> Result<Option<String>, String>,
) -> Result<Option<String>, String> {
    for rel in PROJECT_CONFIG {
        let Some(sha) = sha_of(rel)? else { continue };
        if !repo
            .allow_project_config
            .iter()
            .any(|a| a.path == rel && a.sha256 == sha)
        {
            return Ok(Some(format!(
                "RepoNotCompliant: project agent config {rel}"
            )));
        }
    }
    Ok(None)
}

/// `config_problem` for the files in a worktree.
pub fn project_config_problem(repo: &RepoCfg, worktree: &Path) -> Result<Option<String>, String> {
    config_problem(repo, |rel| {
        let p = worktree.join(rel);
        p.exists()
            .then(|| crate::haruspex::cli::sha256_file(&p))
            .transpose()
    })
}

fn compose(repo: &RepoCfg, cmd: &str) -> String {
    match &repo.factory_compose {
        Some(p) => format!("{p} {cmd}"),
        None => cmd.to_string(),
    }
}

fn fence(text: &str) -> String {
    format!("```text\n{}\n```", text.replace("```", "'''"))
}

/// The class of an incident, the only part of it that goes to GitHub.
pub fn incident_class(reason: &str) -> &'static str {
    ["guard", "approval", "contract", "managed", "token"]
        .into_iter()
        .find(|c| reason.contains(c))
        .unwrap_or("integrity")
}

/// The issue comment for an outcome. LLM-written text is fenced as data, never markdown.
fn comment(repo: &str, n: u64, shift: &str, d: &Done) -> String {
    match d.kind {
        Kind::Pr => format!("Draft PR: {}", d.pr_url.as_deref().unwrap_or("")),
        Kind::NeedsDecision => {
            let qs: Vec<String> = d
                .escalations
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    format!(
                        "**{}** ({}). Recommended: {}\n{}",
                        crate::ids::one_line(e["id"].as_str().unwrap_or(""), 32),
                        e["severity"].as_str().unwrap_or(""),
                        fence(e["recommendation"].as_str().unwrap_or("")),
                        fence(e["question"].as_str().unwrap_or(""))
                    )
                })
                .collect();
            format!(
                "workgraph needs a decision before any edit.\n\n{}\n\nThe owner answers with `/workgraph:shift decide {repo}#{n}`; comments are never read as decisions.",
                qs.join("\n\n")
            )
        }
        Kind::Incident => {
            let class = incident_class(&d.reason);
            format!("An incident aborted this task (class: {class}). The shift stopped; details are in the owner's journal.")
        }
        Kind::Parked if d.resumable() => format!(
            "workgraph parked this task when the shift stopped; it stays claimed. Resume it with `wg shift resume {}`:\n\n{}",
            shift,
            fence(&d.reason)
        ),
        Kind::Stuck | Kind::Parked => {
            format!("workgraph {}:\n\n{}", d.kind.name(), fence(&d.reason))
        }
    }
}

/// Labels and comment for the outcome, then the record (design §7.3 step 8).
fn write_back(
    e: &Env,
    repo: &str,
    n: u64,
    rec: TaskRecord,
    d: &Done,
) -> Result<TaskRecord, String> {
    let (add, label_out): (&str, &str) = match d.kind {
        Kind::Pr => ("wg:pr-open", "pr-open"),
        Kind::NeedsDecision => ("wg:needs-decision", "needs-decision"),
        Kind::Stuck => ("wg:stuck", "stuck"),
        // Still claimed by this shift: no other shift takes it, `wg shift resume` does.
        Kind::Parked if d.resumable() => (IN_SHIFT, "in-shift"),
        Kind::Parked => (e.cfg.shift.label.as_str(), "parked"),
        Kind::Incident => ("wg:incident", "incident"),
    };
    if add != IN_SHIFT {
        intake::relabel(e.dirs, e.gh, repo, n, &[add], &[IN_SHIFT])?;
    }
    e.gh.comment(repo, n, &comment(repo, n, e.shift_id, d))?;
    let pr = d
        .pr_url
        .as_deref()
        .and_then(|u| u.rsplit('/').next())
        .and_then(|s| s.parse().ok());
    let rec = TaskRecord {
        status: label_out.into(),
        pr: pr.or(rec.pr),
        pr_url: d.pr_url.clone().or(rec.pr_url),
        updated_at: now(),
        ..rec
    };
    record::save(e.dirs, &rec)?;
    Ok(rec)
}

/// Works one issue. `resume` replays the record's last run and skips the claim.
pub fn work(e: &Env, repo: &RepoCfg, i: &Issue, resume: bool) -> Result<Done, String> {
    let existing = record::load(e.dirs, &repo.name, i.number)?;
    if !resume {
        intake::claim(e.dirs, e.gh, &e.cfg.shift, &repo.name, i.number, e.shift_id)?;
    }
    // Journalled before the worktree and the install, so a shift killed there is resumable.
    let run_id = format!("r-{}", tokens_store::random_hex(6)?);
    e.journal.append(
        "task_claimed",
        json!({ "repo": repo.name, "issue": i.number, "run": run_id, "resume": resume }),
    )?;
    let branch = existing
        .as_ref()
        .map(|r| r.branch.clone())
        .unwrap_or_else(|| format!("wg/{}-{}", i.number, issue::slug(&i.title)));
    let rel = format!(
        "{}/{}",
        record::slug(&repo.name),
        branch.trim_start_matches("wg/")
    );
    let worktree = git::worktree(
        Path::new(&repo.path),
        &e.dirs.worktrees(),
        &rel,
        &branch,
        &repo.default_branch,
    )?;
    let rec = TaskRecord {
        repo: repo.name.clone(),
        issue: i.number,
        title: i.title.clone(),
        worktree: worktree.to_string_lossy().into_owned(),
        branch: branch.clone(),
        body_sha256: issue::body_sha256(i),
        status: "in-shift".into(),
        claimed_at: now(),
        updated_at: now(),
        shift: e.shift_id.to_string(),
        ..existing.clone().unwrap_or_default()
    };
    record::save(e.dirs, &rec)?;
    // A backstop to the repo preflight: the claim is released, and it is no task failure.
    if let Some(problem) = project_config_problem(repo, &worktree)? {
        let d = done(Kind::Parked, None, problem);
        write_back(e, &repo.name, i.number, rec, &d)?;
        return Ok(d);
    }
    let rec = match (&repo.install, rec.installed) {
        (Some(install), false) => {
            let (code, tail) = crate::exec::run::run_shell_in(
                &compose(repo, install),
                &worktree,
                &crate::exec::env::scrubbed(),
                INSTALL_TIMEOUT,
            );
            e.journal.append(
                "install",
                json!({ "repo": repo.name, "issue": i.number, "exit": code }),
            )?;
            if code != 0 {
                let d = done(
                    Kind::Stuck,
                    None,
                    format!("install failed (exit {code}):\n{tail}"),
                );
                write_back(e, &repo.name, i.number, rec, &d)?;
                return Ok(d);
            }
            TaskRecord {
                installed: true,
                ..rec
            }
        }
        _ => rec,
    };
    let replay = match (resume, rec.runs.last()) {
        (true, Some(last)) => {
            match Replay::load(
                &e.dirs.run(last),
                &crate::claude::replay::binary_hash(),
                &crate::assets::hash(),
            ) {
                Ok(r) => r,
                Err(why) => {
                    e.journal.append("warning", json!({ "issue": i.number, "warning": format!("rerun from scratch: {why}") }))?;
                    Replay::none()
                }
            }
        }
        _ => Replay::none(),
    };
    let floor = first_baseline(e.dirs, &rec.runs);
    let tainted_earlier = tainted_earlier(e.dirs, &rec.runs);
    let rec = TaskRecord {
        runs: rec.runs.iter().cloned().chain([run_id.clone()]).collect(),
        ..rec
    };
    record::save(e.dirs, &rec)?;
    let (d, ran_rec) = run(
        e,
        repo,
        i,
        rec,
        &worktree,
        &run_id,
        replay,
        floor,
        tainted_earlier,
    )?;
    write_back(e, &repo.name, i.number, ran_rec, &d)?;
    Ok(d)
}

/// The checks that failed at this task's first baseline, which ran on the fresh worktree before
/// any Worker edit (the kernel orders the baseline first). `None` before any baseline ran.
pub fn first_baseline(dirs: &Dirs, runs: &[String]) -> Option<Vec<String>> {
    runs.iter().find_map(|r| {
        let journal = crate::journal::read(&dirs.run(r).join("journal.jsonl")).ok()?;
        let e = journal
            .iter()
            .find(|e| e["kind"] == "checks_run" && e["label"] == "check:baseline")?;
        Some(
            e["results"]
                .as_array()?
                .iter()
                .filter(|x| x["exit"] != 0)
                .filter_map(|x| x["name"].as_str().map(str::to_string))
                .collect(),
        )
    })
}

/// Whether an earlier run of this task tainted it (a risky unit was built, or a unit turned risky):
/// its edits stay in the kept worktree, so the rerun is tainted from the start (design §8.1). A run
/// with no journal tainted nothing.
pub fn tainted_earlier(dirs: &Dirs, runs: &[String]) -> bool {
    runs.iter().any(|r| {
        crate::journal::read(&dirs.run(r).join("journal.jsonl")).is_ok_and(|j| {
            j.iter()
                .any(|e| e["kind"] == "risk_updated" && e["tainted"] == true)
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn run(
    e: &Env,
    repo: &RepoCfg,
    i: &Issue,
    rec: TaskRecord,
    worktree: &Path,
    run_id: &str,
    replay: Replay,
    baseline_floor: Option<Vec<String>>,
    tainted_earlier: bool,
) -> Result<(Done, TaskRecord), String> {
    let slug = record::slug(&repo.name);
    let memory_path = e.dirs.memory(&slug);
    let _lock = memory_store::Lock::take(&memory_path)?;
    let chain = shift_chain(e.cfg, repo)?;
    let decisions_path = e.dirs.decisions(&slug, i.number);
    let decisions = crate::cli::run::decisions(
        decisions_path
            .exists()
            .then(|| decisions_path.to_string_lossy().into_owned())
            .as_deref(),
    )?;
    let place = runctx::Place {
        issue: i.number,
        branch: rec.branch.clone(),
        pipeline: repo.pipelines.join("+"),
    };
    let (ran, out) = runctx::with_ctx(e, repo, worktree, run_id, replay, &place, |ctx| {
        let start = Start {
            task: issue::fenced(i),
            title: i.title.clone(),
            decisions,
            resume_task: rec.task_id.clone(),
            initialized_earlier: rec.initialized,
            // The owner's checkout is never touched, so nothing in the worktree is the owner's work.
            user_changes_before: vec![],
            memory: memory_store::load(&memory_path),
            memory_path: memory_path.clone(),
            chain,
            start_branch: rec.branch.clone(),
            initializing: !worktree.join(".haruspex").exists(),
            baseline_floor,
            tainted_earlier,
        };
        runctx::finish(
            ctx,
            driver::run_task(ctx, start, &wg_factory::engine::step)?,
        )
    })?;
    let task_id = Some(ran.kernel.task_id())
        .filter(|t| !t.is_empty())
        .or(rec.task_id.clone());
    let rec = TaskRecord {
        task_id,
        initialized: rec.initialized || out["initialized"] == true,
        ..rec
    };
    Ok((outcome(&ran, &out, run_id), rec))
}

/// A shift task's chain: the repo's pipelines, which must publish (design §6.2, Phase 4).
pub fn shift_chain(cfg: &Config, repo: &RepoCfg) -> Result<wg_factory::api::Chain, String> {
    let chain = crate::pipelines::chain(
        &crate::assets::Assets::compiled(),
        &repo.pipelines,
        &crate::pipelines::RepoView::of(repo),
        cfg.max_calls_per_task,
    )?;
    if chain.publish().is_none() {
        return Err(format!(
            "the chain {} has no publish stage, so a shift could never open a PR",
            repo.pipelines.join("+")
        ));
    }
    Ok(chain)
}

/// Failing check tails from the kernel's own results, for a stuck comment.
fn tails(ran: &driver::Ran) -> String {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for c in ran.kernel.checks.iter().rev() {
        if !seen.contains(&c.name()) {
            seen.push(c.name());
            if !c.passed() {
                out.push(format!(
                    "{} (exit {}):\n{}",
                    c.name(),
                    c.exit_code(),
                    c.output_tail()
                ));
            }
        }
    }
    out.join("\n\n")
}

fn outcome(ran: &driver::Ran, out: &Value, run_id: &str) -> Done {
    let run = Some(run_id);
    let note = out["note"].as_str().unwrap_or("").to_string();
    match &ran.end {
        End::Incident(why) => done(Kind::Incident, run, why.clone()),
        // A draft PR exists: the task is a PR whatever ended the run after it.
        _ if ran.published.is_some() => Done {
            pr_url: ran.published.clone(),
            ..done(Kind::Pr, run, "draft PR opened")
        },
        End::NeedsDecision(_) => Done {
            escalations: out["escalations"].clone(),
            ..done(Kind::NeedsDecision, run, "needs a decision")
        },
        End::Parked(why) => Done {
            rate_limited: why.starts_with("rate limited"),
            ..done(Kind::Parked, run, why.clone())
        },
        End::Failed(why) => done(Kind::Stuck, run, format!("{why}\n\n{}", tails(ran))),
        // A dev chain has no human stage; the release line reads its own outcome (release.rs).
        End::AwaitingOwner { ask, .. } => done(Kind::Stuck, run, ask.clone()),
        End::Finish(_) => {
            let status = out["status"].as_str().unwrap_or("");
            let why = if note.is_empty() {
                status.to_string()
            } else {
                note
            };
            done(
                Kind::Stuck,
                run,
                format!("{why}\n\n{}", tails(ran)).trim().to_string(),
            )
        }
    }
}

/// The run dir of a task's last run.
pub fn last_run(dirs: &Dirs, rec: &TaskRecord) -> Option<PathBuf> {
    rec.runs.last().map(|r| dirs.run(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_fence_agent_text_and_name_only_the_incident_class() {
        let mut d = done(Kind::NeedsDecision, None, "x");
        d.escalations = json!([{ "id": "E1", "severity": "critical", "question": "drop ```table``` @everyone?", "recommendation": "no" }]);
        let c = comment("o/r", 3, "s1", &d);
        assert!(
            c.contains("```text\ndrop '''table''' @everyone?\n```"),
            "{c}"
        );
        assert!(c.contains("/workgraph:shift decide o/r#3"));
        let i = comment(
            "o/r",
            3,
            "s1",
            &done(
                Kind::Incident,
                None,
                "guard incident in work:a: rm -rf secret path",
            ),
        );
        assert!(i.contains("class: guard") && !i.contains("secret path"));
        assert!(
            comment("o/r", 3, "s1", &done(Kind::Stuck, None, "why")).contains("```text\nwhy\n```")
        );
        let parked = comment(
            "o/r",
            3,
            "s1",
            &done(Kind::Parked, None, "shift stopped: the STOP file exists"),
        );
        assert!(parked.contains("`wg shift resume s1`"), "{parked}");
        assert!(!done(Kind::Parked, None, "call cap").resumable());
        let mut pr = done(Kind::Pr, None, "");
        pr.pr_url = Some("https://x/pull/4".into());
        assert_eq!(comment("o/r", 3, "s1", &pr), "Draft PR: https://x/pull/4");
    }

    #[test]
    fn stop_reasons_and_project_config() {
        let dir = std::env::temp_dir().join(format!("wg-task-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        assert_eq!(stop_reason(&dir.join("STOP"), now() + 60), None);
        assert!(stop_reason(&dir.join("STOP"), now())
            .unwrap()
            .contains("window"));
        std::fs::write(dir.join("STOP"), "").unwrap();
        assert!(stop_reason(&dir.join("STOP"), now() + 60)
            .unwrap()
            .contains("STOP"));
        let repo: RepoCfg = toml::from_str("name = \"o/r\"\npath = \"/x\"").unwrap();
        assert_eq!(project_config_problem(&repo, &dir).unwrap(), None);
        std::fs::write(dir.join(".mcp.json"), "{}").unwrap();
        assert!(project_config_problem(&repo, &dir)
            .unwrap()
            .unwrap()
            .contains(".mcp.json"));
        let sha = crate::haruspex::cli::sha256_file(&dir.join(".mcp.json")).unwrap();
        let allowed: RepoCfg = toml::from_str(&format!(
            "name = \"o/r\"\npath = \"/x\"\nallow_project_config = [{{ path = \".mcp.json\", sha256 = \"{sha}\" }}]"
        ))
        .unwrap();
        assert_eq!(project_config_problem(&allowed, &dir).unwrap(), None);
        assert_eq!(compose(&allowed, "run x"), "run x");
    }

    #[test]
    fn the_first_baseline_of_a_task_bounds_what_a_rerun_may_leave_out() {
        let home = std::env::temp_dir().join(format!("wg-task-floor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dirs = Dirs { home };
        let runs = ["r-a", "r-b", "r-c"].map(String::from);
        assert_eq!(first_baseline(&dirs, &runs), None);
        let journal = |run: &str, results: Value| {
            std::fs::create_dir_all(dirs.run(run)).unwrap();
            let j = Journal::open(&dirs.run(run).join("journal.jsonl"), run).unwrap();
            j.append("checks_run", json!({ "label": "check:L1", "results": [] }))
                .unwrap();
            j.append(
                "checks_run",
                json!({ "label": "check:baseline", "results": results }),
            )
            .unwrap();
        };
        // r-a never reached its baseline (no journal); r-b's is the first; r-c ran on edits.
        journal(
            "r-b",
            json!([{ "name": "test", "exit": 0 }, { "name": "lint", "exit": 1 }]),
        );
        journal(
            "r-c",
            json!([{ "name": "test", "exit": 1 }, { "name": "lint", "exit": 1 }]),
        );
        assert_eq!(first_baseline(&dirs, &runs), Some(vec!["lint".to_string()]));
    }

    #[test]
    fn a_rerun_is_tainted_when_any_earlier_run_was() {
        let home = std::env::temp_dir().join(format!("wg-task-taint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dirs = Dirs { home };
        let runs = ["r-a", "r-b"].map(String::from);
        let journal = |run: &str, tainted: bool| {
            std::fs::create_dir_all(dirs.run(run)).unwrap();
            let j = Journal::open(&dirs.run(run).join("journal.jsonl"), run).unwrap();
            j.append("risk_updated", json!({ "risky": [], "tainted": tainted }))
                .unwrap();
        };
        assert!(!tainted_earlier(&dirs, &runs), "no journals");
        journal("r-a", false);
        assert!(!tainted_earlier(&dirs, &runs));
        journal("r-b", true);
        assert!(tainted_earlier(&dirs, &runs));
    }
}
