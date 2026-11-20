//! `wg sandbox ...` on the Mac (design §8.4-§8.6):
//!
//!   doctor                                   the Mac checks plus the agent's; exit 1 when not ok
//!   reap [--older-than S]                    labelled jobs older than S seconds (default 2 h)
//!   setup-print                              the provisioning plan and script the owner runs
//!   exec --unit U [--patch] -- CMD...        a risky Worker or Fixer's command, in the sandbox
//!   exec --task T -- CMD...                  a tainted Reviewer's command, in the sandbox
//!   exec --task T --check NAME               Haruspex verify of a tainted task (project.json)
//!   poll --job J                             a job `exec` left pending
//!
//! `exec` with `--unit` or `--task` alone runs only under the grant in the manifest the kernel
//! wrote for this call (`WG_GUARD_MANIFEST`); the sandbox restricts the environment, not the
//! command. One JSON line; exit 0 when every step passed, 1 when one failed, 2 on error.

use super::args::parse;
use crate::guard::hook::{Manifest, SandboxGrant};
use crate::sandbox::client::{new_job, Client, EXEC_CAP};
use crate::sandbox::job::{self, Job, Polled, Step};
use crate::state_dirs::{private_dir, Dirs};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A step's wall time inside the job (the agent's ceiling is 20 minutes for the whole job).
const STEP_SECS: u32 = 600;

/// The manifest this agent call runs under, only from the agent-denied run directory.
fn manifest(dirs: &Dirs) -> Result<(Manifest, PathBuf), String> {
    let p = std::env::var("WG_GUARD_MANIFEST")
        .map_err(|_| "no WG_GUARD_MANIFEST: exec runs only inside a factory call".to_string())?;
    let p = Path::new(&p)
        .canonicalize()
        .map_err(|e| format!("the manifest is unreadable: {e}"))?;
    let runs = dirs
        .state()
        .join("runs")
        .canonicalize()
        .map_err(|e| format!("no runs directory: {e}"))?;
    let guard = p
        .parent()
        .filter(|d| d.file_name().is_some_and(|n| n == "guard"));
    let run_dir = guard
        .and_then(Path::parent)
        .filter(|r| r.parent() == Some(runs.as_path()));
    let Some(run_dir) = run_dir else {
        return Err("the manifest is not a kernel-written one".into());
    };
    let text =
        std::fs::read_to_string(&p).map_err(|e| format!("the manifest is unreadable: {e}"))?;
    let m: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("the manifest is not valid: {e}"))?;
    Ok((m, run_dir.to_path_buf()))
}

fn exec_cap() -> Duration {
    // A test build may shorten the cap to prove a long job returns `pending` (design §15.4).
    let test = cfg!(debug_assertions)
        .then(|| std::env::var("WG_SANDBOX_EXEC_CAP_SECS").ok()?.parse().ok())
        .flatten();
    test.map(Duration::from_secs).unwrap_or(EXEC_CAP)
}

fn code(v: &Value) -> i32 {
    if v["pending"] == true {
        return 0;
    }
    let failed = v["error"].is_string()
        || v["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["exit"] != 0);
    i32::from(failed)
}

/// The answer for the agent; a returned patch is applied only within the unit's files.
fn finish(
    p: Polled,
    j: &Job,
    worktree: &Path,
    grant: &SandboxGrant,
    run_dir: &Path,
) -> Result<Value, String> {
    let r = match p {
        Polled::Pending => {
            return Ok(
                json!({ "job": j.job, "pending": true, "next": "wg sandbox poll --job <this job>" }),
            )
        }
        Polled::Done(r) => r,
    };
    let mut out = json!({ "job": r.job, "steps": r.steps, "cleanup": r.cleanup, "error": r.error });
    if let Some(patch) = &r.patch {
        out["patch"] = match crate::sandbox::patch::apply(worktree, patch, &grant.files) {
            Ok(paths) => json!({ "applied": paths }),
            Err(why) => json!({ "refused": why }),
        };
    }
    let line = json!({ "job": r.job, "nonce": r.nonce, "unit": grant.unit, "task": grant.task,
                       "steps": r.steps.iter().map(|s| json!({ "name": s.name, "exit": s.exit })).collect::<Vec<_>>(),
                       "cleanup": r.cleanup, "error": r.error, "ts": crate::journal::now() });
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("sandbox.jsonl"))
        .and_then(|mut f| writeln!(f, "{line}"))
        .map_err(|e| format!("cannot record the sandbox run: {e}"))?;
    Ok(out)
}

fn granted_exec(
    dirs: &Dirs,
    unit: Option<&str>,
    task: Option<&str>,
    patch: bool,
    cmd: &[String],
) -> Result<Value, String> {
    let (m, run_dir) = manifest(dirs)?;
    let g = m.sandbox.clone().ok_or("this call has no sandbox grant")?;
    if g.unit.as_deref() != unit || g.task.as_deref() != task {
        return Err("this call's grant is for another unit or task".into());
    }
    if patch && unit.is_none() {
        return Err("--patch is for a unit's own files".into());
    }
    let command = cmd.join(" ");
    job::command(&command, "the command")?;
    let cfg = super::run::load_config(dirs)?;
    let sb = cfg
        .repo(&g.repo)
        .and_then(|r| r.sandbox.clone())
        .ok_or("this repo has no [repo.sandbox]")?;
    let who = unit.or(task).unwrap_or("");
    let j = new_job(
        &sb,
        vec![Step {
            name: "exec".into(),
            cmd: command,
            timeout_secs: STEP_SECS,
        }],
        &format!("{}:{who}:sbx", g.run),
        patch,
    )?;
    let pending = run_dir.join("sandbox");
    private_dir(&pending)?;
    std::fs::write(
        pending.join(format!("{}.json", j.job)),
        serde_json::to_vec(&j).expect("serializes"),
    )
    .map_err(|e| format!("cannot keep the job: {e}"))?;
    let client = Client::new(&cfg);
    client.submit(&j, Path::new(&m.worktree))?;
    finish(
        client.wait(&j, exec_cap())?,
        &j,
        Path::new(&m.worktree),
        &g,
        &run_dir,
    )
}

fn poll(dirs: &Dirs, id: &str) -> Result<Value, String> {
    job::job_id(id)?;
    let (m, run_dir) = manifest(dirs)?;
    let g = m.sandbox.clone().ok_or("this call has no sandbox grant")?;
    let text = std::fs::read(run_dir.join("sandbox").join(format!("{id}.json")))
        .map_err(|_| format!("no job {id} in this run"))?;
    let j: Job =
        serde_json::from_slice(&text).map_err(|e| format!("the kept job is corrupt: {e}"))?;
    let cfg = super::run::load_config(dirs)?;
    finish(
        Client::new(&cfg).wait(&j, exec_cap())?,
        &j,
        Path::new(&m.worktree),
        &g,
        &run_dir,
    )
}

/// Haruspex verify of a tainted task: the named sandbox check on this worktree, output on stdout,
/// the step's exit code as ours.
fn check(dirs: &Dirs, task: &str, name: &str) -> Result<(String, i32), String> {
    crate::ids::task_id(task)?;
    let root = super::run::repo_root()?;
    if !root
        .join(".haruspex/tasks")
        .join(format!("{task}.json"))
        .exists()
    {
        return Err(format!("task {task} is not this worktree's"));
    }
    let cfg = super::run::load_config(dirs)?;
    let sb = cfg
        .repo_at(&root)
        .and_then(|r| r.sandbox.clone())
        .ok_or("this checkout is no [[repo]] with a [repo.sandbox]")?;
    let cmd = sb
        .checks
        .get(name)
        .ok_or_else(|| format!("[repo.sandbox] has no check {name}"))?;
    let j = new_job(
        &sb,
        vec![Step {
            name: name.into(),
            cmd: cmd.clone(),
            timeout_secs: STEP_SECS,
        }],
        &format!("verify:{task}:{name}"),
        false,
    )?;
    let client = Client::new(&cfg);
    client.submit(&j, &root)?;
    let wall = Duration::from_secs(u64::from(j.limits.wall_secs) + 120);
    match client.wait(&j, wall)? {
        Polled::Pending => Err(format!("sandbox job {} did not finish", j.job)),
        Polled::Done(r) => match (&r.error, r.steps.first()) {
            (Some(e), _) => Err(format!("sandbox unavailable: {e}")),
            (None, Some(s)) => Ok((
                format!(
                    "{}\n[wg sandbox: job {} on {}, exit {}]",
                    s.tail, r.job, cfg.sandbox.machine, s.exit
                ),
                s.exit.clamp(0, 255) as i32,
            )),
            (None, None) => Err("the sandbox ran no step".into()),
        },
    }
}

fn run(args: &[String]) -> Result<(Value, i32), String> {
    let dirs = Dirs::from_env()?;
    let (flags, cmd) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    let rest = flags.get(1..).unwrap_or(&[]);
    match flags.first().map(String::as_str) {
        Some("doctor") => {
            parse(rest, &[])?.only(&[])?;
            let cfg = super::run::load_config(&dirs)?;
            let d = crate::sandbox::doctor::doctor(&cfg);
            let ok = d["ok"] == true;
            Ok((d, if ok { 0 } else { 1 }))
        }
        Some("reap") => {
            let a = parse(rest, &[])?;
            a.only(&["older-than"])?;
            let secs = a
                .get("older-than")
                .map(str::parse::<u64>)
                .transpose()
                .map_err(|_| "--older-than takes seconds")?;
            let cfg = super::run::load_config(&dirs)?;
            Ok((
                Client::new(&cfg).reap(secs.unwrap_or(crate::shift::sandbox::REAP_AFTER_SECS))?,
                0,
            ))
        }
        Some("poll") => {
            let a = parse(rest, &[])?;
            a.only(&["job"])?;
            let v = poll(&dirs, a.need("job")?)?;
            let c = code(&v);
            Ok((v, c))
        }
        Some("exec") => {
            let a = parse(rest, &["patch"])?;
            a.only(&["unit", "task", "check", "patch"])?;
            match (a.get("unit"), a.get("task"), a.get("check")) {
                (None, Some(t), Some(n)) if cmd.is_empty() && !a.on("patch") => {
                    let (text, c) = check(&dirs, t, n)?;
                    Ok((Value::String(text), c))
                }
                (Some(_), None, None) | (None, Some(_), None) if !cmd.is_empty() => {
                    let v = granted_exec(&dirs, a.get("unit"), a.get("task"), a.on("patch"), cmd)?;
                    let c = code(&v);
                    Ok((v, c))
                }
                _ => Err("exec takes --unit U [--patch] -- CMD, --task T -- CMD, or --task T --check NAME".into()),
            }
        }
        _ => Err("usage: wg sandbox doctor | reap | setup-print | exec ... | poll --job J".into()),
    }
}

pub fn main(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("setup-print") {
        return match Dirs::from_env().and_then(|d| super::run::load_config(&d)) {
            Ok(cfg) => {
                print!(
                    "{}",
                    crate::sandbox::setup::print(&cfg, env!("CARGO_PKG_VERSION"))
                );
                0
            }
            Err(e) => {
                println!("{}", json!({ "error": e }));
                2
            }
        };
    }
    match run(args) {
        // `--check` prints the step's output as text for Haruspex; everything else one JSON line.
        Ok((Value::String(text), c)) => {
            println!("{text}");
            c
        }
        Ok((v, c)) => {
            println!("{v}");
            c
        }
        Err(e) => {
            println!("{}", json!({ "error": e }));
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_errors_are_one_json_line_and_exit_2() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(main(&a(&["nope"])), 2);
        assert_eq!(main(&a(&["exec", "--unit", "U1"])), 2, "no command");
        assert_eq!(
            main(&a(&[
                "exec", "--task", "T-001", "--check", "test", "--", "x"
            ])),
            2
        );
        assert_eq!(code(&json!({ "pending": true })), 0);
        assert_eq!(code(&json!({ "steps": [{ "exit": 0 }] })), 0);
        assert_eq!(code(&json!({ "steps": [{ "exit": 3 }] })), 1);
        assert_eq!(code(&json!({ "steps": [], "error": "x" })), 1);
    }
}
