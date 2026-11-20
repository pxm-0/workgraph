//! Runs checks by name with the owner-approved `factory` string, never an LLM's, and mints a
//! `CheckResult` token per check at the tree they ran against (design §4.1, §9.3). A tainted task's
//! checks run in the sim sandbox with the `[repo.sandbox].checks` strings instead, and never fall
//! back to the Mac (design §8.1).

use crate::engine::RunCtx;
use crate::sandbox::job::Step;
use crate::tokens::{CheckResult, SandboxRun};
use serde_json::json;
use std::time::Duration;
use wg_factory::api::CheckOutcome;

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(600);

/// The results, the changed files and, in the sandbox, the job's evidence.
pub type Ran = (Vec<CheckResult>, Vec<String>, Option<SandboxRun>);

/// The results and the changed files, or why no check could run (the run then stops, or parks
/// when the sandbox is what failed: such errors start with `sandbox unavailable`).
pub fn run(ctx: &RunCtx, label: &str, names: &[String], tainted: bool) -> Result<Ran, String> {
    if tainted {
        return in_sandbox(ctx, label, names);
    }
    let mut commands = Vec::new();
    for n in names {
        let entry = ctx
            .cfg
            .check(&ctx.repo_name, n)
            .ok_or_else(|| format!("check {n} has no approved command for {}", ctx.repo_name))?;
        commands.push((n.clone(), entry.factory.clone()));
    }
    let mut raw = Vec::new();
    for (name, cmd) in &commands {
        let (code, tail) = ctx.ports.checks.run(label, cmd, CHECK_TIMEOUT)?;
        raw.push((name.clone(), cmd.clone(), code, tail));
    }
    let tree = ctx.ports.repo.tree()?;
    let changed = ctx.ports.repo.changed()?;
    let results: Vec<CheckResult> = raw
        .iter()
        .map(|(n, c, code, tail)| CheckResult::new(n, c, *code, tail, &tree, label))
        .collect();
    journal(ctx, label, &tree, &results)?;
    Ok((results, changed, None))
}

fn journal(ctx: &RunCtx, label: &str, tree: &str, results: &[CheckResult]) -> Result<(), String> {
    ctx.journal.append(
        "checks_run",
        json!({ "label": label, "tree": tree,
                "target": results.first().map_or("host", |r| r.target().name()),
                "results": results.iter().map(|r| json!({ "name": r.name(), "exit": r.exit_code() })).collect::<Vec<_>>() }),
    )?;
    Ok(())
}

/// One job with a step per check; the tree is taken before the work is bundled, so the tokens name
/// exactly what the sandbox ran.
fn in_sandbox(ctx: &RunCtx, label: &str, names: &[String]) -> Result<Ran, String> {
    let sb = ctx
        .cfg
        .repo(&ctx.repo_name)
        .and_then(|r| r.sandbox.as_ref())
        .ok_or("sandbox unavailable: this repo has no [repo.sandbox]")?;
    let mut steps = Vec::new();
    for n in names {
        let cmd = sb.checks.get(n).ok_or_else(|| {
            format!("sandbox unavailable: [repo.sandbox] has no command for check {n}")
        })?;
        steps.push(Step {
            name: n.clone(),
            cmd: cmd.clone(),
            timeout_secs: CHECK_TIMEOUT.as_secs() as u32,
        });
    }
    let tree = ctx.ports.repo.tree()?;
    let changed = ctx.ports.repo.changed()?;
    let r = ctx
        .ports
        .sandbox
        .run(&format!("{}:{label}", ctx.run_id), steps)?;
    let results: Vec<CheckResult> = r
        .steps
        .iter()
        .map(|s| {
            let cmd = sb.checks.get(&s.name).cloned().unwrap_or_default();
            CheckResult::in_sandbox(&s.name, &cmd, s.exit, &s.tail, &tree, label)
        })
        .collect();
    let evidence = SandboxRun::new(&r, label, &tree, &sb.image);
    ctx.journal.append(
        "sandbox_run",
        json!({ "label": label, "job": r.job, "nonce": r.nonce, "tree": tree,
                "steps": r.steps.iter().map(|s| json!({ "name": s.name, "exit": s.exit, "timed_out": s.timed_out, "oom": s.oom })).collect::<Vec<_>>(),
                "cleanup": r.cleanup }),
    )?;
    journal(ctx, label, &tree, &results)?;
    Ok((results, changed, Some(evidence)))
}

/// The factory's plain-data copy; it gains nothing by inventing one.
pub fn outcomes(results: &[CheckResult]) -> Vec<CheckOutcome> {
    results
        .iter()
        .map(|r| CheckOutcome {
            name: r.name().into(),
            cmd: r.command().into(),
            exit_code: r.exit_code(),
            output_tail: r.output_tail().into(),
        })
        .collect()
}
