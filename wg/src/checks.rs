//! Runs checks by name with the owner-approved `factory` string, never an LLM's, and mints a
//! `CheckResult` token per check at the tree they ran against (design §4.1, §9.3).

use crate::engine::RunCtx;
use crate::tokens::CheckResult;
use serde_json::json;
use std::time::Duration;
use wg_factory::api::CheckOutcome;

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(600);

/// The results and the changed files, or why no check could run (the run then stops).
pub fn run(
    ctx: &RunCtx,
    label: &str,
    names: &[String],
) -> Result<(Vec<CheckResult>, Vec<String>), String> {
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
    ctx.journal.append(
        "checks_run",
        json!({ "label": label, "tree": tree,
                "results": results.iter().map(|r| json!({ "name": r.name(), "exit": r.exit_code() })).collect::<Vec<_>>() }),
    )?;
    Ok((results, changed))
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
