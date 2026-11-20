//! The shift's sandbox bookkeeping (design §7.3 steps 3 and 12, §8.4 check 6): the doctor once at
//! shift start (non-fatal: risky units park), the reaper at start and end, and the read-only argus
//! fingerprint before and after, whose change is an incident.

use crate::config::Config;
use crate::journal::Journal;
use crate::sandbox::client::{Client, Status};
use crate::sandbox::doctor;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// Jobs older than this are the reaper's (design §8.5 step 8).
pub const REAP_AFTER_SECS: u64 = 2 * 3600;

pub struct Shift {
    pub status: Status,
    /// The fingerprint at start, when a sandbox is in use.
    pub argus: Option<Value>,
}

fn reap(cfg: &Config, j: &Journal, at: &str) -> Result<(), String> {
    match Client::new(cfg).reap(REAP_AFTER_SECS) {
        Ok(v) => j.append("sandbox_reap", json!({ "at": at, "result": v })),
        Err(e) => j.append(
            "warning",
            json!({ "warning": format!("sandbox reap at {at}: {e}") }),
        ),
    }
    .map(|_| ())
}

pub fn start(cfg: &Config, j: &Journal) -> Result<Shift, String> {
    if !cfg.repos.iter().any(|r| r.sandbox.is_some()) {
        let why = "no [[repo]] has a [repo.sandbox]".to_string();
        j.append(
            "sandbox_doctor",
            json!({ "ok": false, "failed": why, "skipped": true }),
        )?;
        return Ok(Shift {
            status: Arc::new(Mutex::new(Some(Err(why)))),
            argus: None,
        });
    }
    let d = doctor::doctor(cfg);
    j.append(
        "sandbox_doctor",
        json!({ "ok": d["ok"], "failed": d["failed"], "vm_host_reach": d["vm_host_reach"], "checks": d["checks"] }),
    )?;
    let verdict = match d["failed"].as_str() {
        None if d["ok"] == true => Ok(()),
        other => Err(other.unwrap_or("the sandbox doctor failed").to_string()),
    };
    if verdict.is_ok() {
        reap(cfg, j, "start")?;
    }
    let argus = match doctor::fingerprint(cfg) {
        Ok(f) => {
            j.append(
                "argus_fingerprint",
                json!({ "at": "start", "fingerprint": f }),
            )?;
            Some(f)
        }
        Err(e) => {
            j.append(
                "warning",
                json!({ "warning": format!("argus fingerprint at start: {e}") }),
            )?;
            None
        }
    };
    Ok(Shift {
        status: Arc::new(Mutex::new(Some(verdict))),
        argus,
    })
}

pub fn end(cfg: &Config, j: &Journal, s: &Shift) -> Result<(), String> {
    let Some(before) = &s.argus else {
        return Ok(());
    };
    if s.status
        .lock()
        .map(|v| matches!(*v, Some(Ok(()))))
        .unwrap_or(false)
    {
        reap(cfg, j, "end")?;
    }
    match doctor::fingerprint(cfg) {
        Ok(after) => {
            j.append(
                "argus_fingerprint",
                json!({ "at": "end", "fingerprint": after }),
            )?;
            if let Some(why) = doctor::changed(before, &after) {
                j.append("incident", json!({ "at": "argus", "why": why }))?;
            }
        }
        Err(e) => {
            j.append(
                "warning",
                json!({ "warning": format!("argus fingerprint at end: {e}") }),
            )?;
        }
    }
    Ok(())
}
