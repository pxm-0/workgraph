//! `wg sandbox-agent ...`: the Linux side, the same binary installed root-owned at
//! `/usr/local/lib/wgsbx/wg` on the sim and run as wgsbx through a fixed `orb run` argv.
//!
//!   submit                 job line + tar on stdin; stored, started as a transient user unit
//!   poll --job J           the result, or {"job":J,"pending":true,"queued":<waits for the lock>}
//!   cancel --job J         stops the job's unit (its cleanup still runs)
//!   run [--job J]          runs a stored job (the unit), or one from stdin in the foreground
//!   cleanup --job J        the unit's ExecStopPost: removes what the job left, records a lost run
//!   reap --older-than S    removes labelled objects older than S seconds
//!   doctor | version

pub mod doctor;
pub mod podman;
pub mod probe;
pub mod reap;
pub mod run;
pub mod store;

use crate::cli::args::parse;
use crate::sandbox::job::{Cleanup, JobResult, PROTOCOL};
use podman::RealPodman;
use serde_json::{json, Value};
use store::Store;

fn run_stored(s: &Store, id: &str) -> Result<Value, String> {
    let job = s.load(id)?;
    let _lock = s.lock()?;
    s.mark_started(id)?;
    let r = run::execute(&RealPodman, &job, &s.tar(id)?, crate::journal::now());
    s.save_result(&r)?;
    serde_json::to_value(&r).map_err(|e| e.to_string())
}

/// After the unit's main process: whatever it left is removed, and a run that died without a
/// result (the agent was killed) gets one that says so.
fn cleanup(s: &Store, id: &str) -> Result<Value, String> {
    let job = s.load(id)?;
    let c = run::cleanup(&RealPodman, id, &run::volumes(&job));
    if s.result(id)?.is_none() {
        s.save_result(&JobResult {
            job: job.job.clone(),
            nonce: job.nonce.clone(),
            steps: vec![],
            patch: None,
            cleanup: c.clone(),
            error: Some("the sandbox job ended without a result (the agent was stopped)".into()),
        })?;
    }
    Ok(json!({ "job": id, "cleanup": c }))
}

/// The job's unit still runs or still cleans up (`ExecStopPost` runs while it is `deactivating`).
fn unit_active(id: &str) -> bool {
    std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "-p",
            "ActiveState",
            "--value",
            &format!("wgsbx-{id}"),
        ])
        .env_clear()
        .envs(podman::env())
        .output()
        .is_ok_and(|o| {
            let state = String::from_utf8_lossy(&o.stdout).trim().to_string();
            o.status.success() && !matches!(state.as_str(), "" | "inactive" | "failed")
        })
}

fn poll(s: &Store, id: &str) -> Result<Value, String> {
    if let Some(r) = s.result(id)? {
        return serde_json::from_str(&r).map_err(|e| format!("the stored result is corrupt: {e}"));
    }
    let job = s.load(id)?;
    if unit_active(id) {
        return Ok(json!({ "job": id, "pending": true, "queued": !s.started(id)? }));
    }
    Ok(serde_json::to_value(JobResult {
        job: job.job,
        nonce: job.nonce,
        steps: vec![],
        patch: None,
        cleanup: Cleanup {
            containers_left: u32::MAX,
            volumes_left: u32::MAX,
        },
        error: Some("the sandbox job is gone without a result".into()),
    })
    .expect("serializes"))
}

/// Stops a job the Mac gave up on; `ExecStopPost` still cleans up and records a lost run.
fn cancel(s: &Store, id: &str) -> Result<Value, String> {
    s.load(id)?;
    let out = std::process::Command::new("systemctl")
        .args(["--user", "stop", &format!("wgsbx-{id}")])
        .env_clear()
        .envs(podman::env())
        .output()
        .map_err(|e| format!("systemctl failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemctl could not stop job {id}: {}",
            crate::ids::one_line(&String::from_utf8_lossy(&out.stderr), 300)
        ));
    }
    Ok(json!({ "job": id, "cancelled": true }))
}

/// Starts the stored job detached, so a killed Mac client never skips its cleanup.
fn start_unit(id: &str) -> Result<(), String> {
    let (_, running) = {
        let o = std::process::Command::new("systemctl")
            .args(["--user", "is-system-running"])
            .env_clear()
            .envs(podman::env())
            .output()
            .map_err(|e| format!("systemctl failed to start: {e}"))?;
        (
            o.status,
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        )
    };
    if !matches!(running.as_str(), "running" | "degraded") {
        return Err(format!(
            "the sandbox user's systemd is {running:?}, not running"
        ));
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the agent: {e}"))?;
    let exe = exe.to_string_lossy();
    let out = std::process::Command::new("systemd-run")
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--unit",
            &format!("wgsbx-{id}"),
        ])
        .arg(format!(
            "--property=ExecStopPost={exe} sandbox-agent cleanup --job {id}"
        ))
        .args([exe.as_ref(), "sandbox-agent", "run", "--job", id])
        .env_clear()
        .envs(podman::env())
        .output()
        .map_err(|e| format!("systemd-run failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemd-run refused the job: {}",
            crate::ids::one_line(&String::from_utf8_lossy(&out.stderr), 300)
        ));
    }
    Ok(())
}

fn dispatch(args: &[String]) -> Result<Value, String> {
    if !cfg!(target_os = "linux") {
        return Err("sandbox-agent runs only on the Linux sandbox machine".into());
    }
    let rest = args.get(1..).unwrap_or(&[]);
    let a = parse(rest, &[])?;
    match args.first().map(String::as_str) {
        Some("version") => Ok(json!({ "protocol": PROTOCOL })),
        Some("doctor") => {
            a.only(&[])?;
            // A test build reads a planted breach (design §16 Phase 3); release builds ignore it.
            let planted = if cfg!(debug_assertions) { std::env::var("WGSBX_TEST_PLANT").unwrap_or_default() } else { String::new() };
            Ok(doctor::doctor(&RealPodman, &planted))
        }
        Some("submit") => {
            a.only(&[])?;
            let allow = store::allowlist()?;
            let s = Store::home()?;
            let job = s.receive(&mut std::io::stdin().lock(), &|i| allow.allows(i))?;
            start_unit(&job.job)?;
            Ok(json!({ "job": job.job, "submitted": true }))
        }
        Some("run") => {
            a.only(&["job"])?;
            let s = Store::home()?;
            match a.get("job") {
                Some(id) => run_stored(&s, id),
                None => {
                    let allow = store::allowlist()?;
                    let job = s.receive(&mut std::io::stdin().lock(), &|i| allow.allows(i))?;
                    run_stored(&s, &job.job)
                }
            }
        }
        Some("poll") => poll(&Store::home()?, a.need("job")?),
        Some("cleanup") => cleanup(&Store::home()?, a.need("job")?),
        Some("cancel") => cancel(&Store::home()?, a.need("job")?),
        Some("reap") => {
            a.only(&["older-than"])?;
            let secs: u64 = a.need("older-than")?.parse().map_err(|_| "--older-than takes seconds")?;
            let s = Store::home()?;
            let mut out = reap::reap(&RealPodman, secs, crate::journal::now())?;
            let old = s.older_than(secs);
            for d in &old {
                // A job directory is the agent's own scratch; one that will not go is reported.
                if let Err(e) = std::fs::remove_dir_all(d) {
                    out["failed"].as_array_mut().expect("array").push(json!(format!("{}: {e}", d.display())));
                }
            }
            out["job_dirs"] = json!(old.len());
            Ok(out)
        }
        _ => Err("usage: wg sandbox-agent submit | poll --job J | run [--job J] | cleanup --job J | cancel --job J | reap --older-than S | doctor | version".into()),
    }
}

/// One JSON line on stdout; exit 2 on error.
pub fn main(args: &[String]) -> i32 {
    match dispatch(args) {
        Ok(v) => {
            println!("{v}");
            0
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
    fn bad_usage_is_one_json_error() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(main(&a(&["nope"])), 2);
        assert_eq!(main(&a(&["reap", "--older-than", "x"])), 2);
        assert_eq!(dispatch(&a(&["version"])).unwrap()["protocol"], PROTOCOL);
        assert!(dispatch(&a(&["poll"])).unwrap_err().contains("--job"));
    }
}
