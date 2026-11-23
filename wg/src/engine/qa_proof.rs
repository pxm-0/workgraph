//! A QA finding counts only when a run proves it (design §6.5). The explorer's repro is untrusted
//! agent output, so the kernel reruns it itself, and only in the sim sandbox (no network, no host
//! mounts, §8.3), whatever the task's taint: the Mac runs only owner-approved command strings
//! (§14). A finding stays only if its rerun exits non-zero; the rest are dropped, with a reason in
//! the journal, and `passed` is recomputed from what stayed.

use super::RunCtx;
use crate::sandbox::job::{Step, MAX_STEPS};
use serde_json::json;
use wg_factory::api::{Finding, Verdict};

/// One repro's wall time inside its job.
const REPRO_SECS: u32 = 300;

/// The verdict the factory sees: only proven findings, `passed` unless one is high or critical.
pub fn prove(ctx: &RunCtx, label: &str, v: &Verdict) -> Result<Verdict, String> {
    let mut kept: Vec<Finding> = Vec::new();
    let mut dropped: Vec<(&Finding, String)> = Vec::new();
    let (with, without): (Vec<&Finding>, Vec<&Finding>) =
        v.findings.iter().partition(|f| f.repro.is_some());
    dropped.extend(without.into_iter().map(|f| (f, "no repro".to_string())));
    for (n, chunk) in with.chunks(MAX_STEPS).enumerate() {
        let steps: Vec<Step> = chunk
            .iter()
            .enumerate()
            .map(|(i, f)| Step {
                name: format!("repro-{}", n * MAX_STEPS + i + 1),
                cmd: f.repro.as_ref().map(|r| r.cmd.clone()).unwrap_or_default(),
                timeout_secs: REPRO_SECS,
            })
            .collect();
        let nonce = format!("{}:{label}.repro-{}", ctx.run_id, n + 1);
        let r = match ctx.ports.sandbox.run(&nonce, steps.clone()) {
            Ok(r) => r,
            Err(why) => {
                dropped.extend(chunk.iter().map(|f| (*f, why.clone())));
                continue;
            }
        };
        ctx.journal.append(
            "sandbox_run",
            json!({ "label": label, "what": "qa repro", "job": r.job, "nonce": r.nonce,
                    "steps": r.steps.iter().map(|s| json!({ "name": s.name, "exit": s.exit, "timed_out": s.timed_out, "oom": s.oom })).collect::<Vec<_>>(),
                    "cleanup": r.cleanup }),
        )?;
        for (f, step) in chunk.iter().zip(&steps) {
            let why = match r.steps.iter().find(|s| s.name == step.name) {
                None => "the sandbox returned no result for the repro".to_string(),
                Some(s) if s.timed_out => format!("the repro timed out after {REPRO_SECS} s"),
                Some(s) if s.oom => "the repro ran out of memory".to_string(),
                Some(s) if s.exit == 0 => "the kernel's rerun of the repro exited 0".to_string(),
                Some(_) => {
                    kept.push((*f).clone());
                    continue;
                }
            };
            dropped.push((f, why));
        }
    }
    for (f, why) in &dropped {
        ctx.journal.append(
            "qa_finding_dropped",
            json!({ "label": label, "id": f.id, "severity": f.severity.name(), "reason": why }),
        )?;
    }
    Ok(Verdict {
        passed: !kept.iter().any(|f| f.severity.serious()),
        findings: kept,
    })
}
