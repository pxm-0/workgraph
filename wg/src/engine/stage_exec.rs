//! The kernel side of the Phase 4 stage commands that are not checks or LLM calls: a sandbox step
//! set (the release rehearsal) and what a release writer's reply leaves in the run dir.

use super::command_check;
use super::driver::End;
use super::kernel::Kernel;
use super::RunCtx;
use crate::sandbox::job::Step;
use crate::tokens::SandboxRun;
use serde_json::json;
use std::collections::VecDeque;
use wg_factory::api::{CheckOutcome, Event, Reply};

/// Runs a configured step set in the sim sandbox, never on the Mac (design §8.1): the steps in one
/// job, in order, on the worktree's tracked files. A sandbox that cannot run parks the run.
pub fn sandbox(
    ctx: &RunCtx,
    k: &mut Kernel,
    queue: &mut VecDeque<Event>,
    label: String,
    steps_from: &str,
) -> Result<Option<End>, String> {
    let cmds = match command_check::sandbox_steps(ctx, &label, steps_from) {
        Ok(c) => c,
        Err(r) => {
            ctx.journal.append("warning", json!({ "refused": r.why }))?;
            return Ok(Some(End::Failed(format!(
                "sandbox stage refused: {}",
                r.why
            ))));
        }
    };
    let image = ctx
        .cfg
        .repo(&ctx.repo_name)
        .and_then(|r| r.sandbox.as_ref())
        .map(|s| s.image.clone())
        .unwrap_or_default();
    let steps: Vec<Step> = cmds
        .iter()
        .enumerate()
        .map(|(i, cmd)| Step {
            name: format!("{steps_from}-{}", i + 1).replace('_', "-"),
            cmd: cmd.clone(),
            timeout_secs: crate::checks::CHECK_TIMEOUT.as_secs() as u32,
        })
        .collect();
    let tree = ctx.ports.repo.tree()?;
    let r = match ctx
        .ports
        .sandbox
        .run(&format!("{}:{label}", ctx.run_id), steps.clone())
    {
        Ok(r) => r,
        Err(why) => {
            let why = if why.starts_with("sandbox unavailable") {
                why
            } else {
                format!("sandbox unavailable: {why}")
            };
            ctx.journal
                .append("parked", json!({ "label": label, "reason": why }))?;
            return Ok(Some(End::Parked(why)));
        }
    };
    ctx.journal.append(
        "sandbox_run",
        json!({ "label": label, "job": r.job, "nonce": r.nonce, "tree": tree,
                "steps": r.steps.iter().map(|s| json!({ "name": s.name, "exit": s.exit, "timed_out": s.timed_out, "oom": s.oom })).collect::<Vec<_>>(),
                "cleanup": r.cleanup }),
    )?;
    k.sandbox_runs
        .push(SandboxRun::new(&r, &label, &tree, &image));
    let results = r
        .steps
        .iter()
        .map(|s| CheckOutcome {
            name: s.name.clone(),
            cmd: steps
                .iter()
                .find(|x| x.name == s.name)
                .map(|x| x.cmd.clone())
                .unwrap_or_default(),
            exit_code: s.exit,
            output_tail: s.tail.clone(),
        })
        .collect();
    queue.push_back(Event::SandboxDone { label, results });
    Ok(None)
}

/// Release notes go to `release-notes.md` in the run dir; the plan's verdict to the journal. Both
/// are the model's text: written as data, never run.
pub fn release_reply(ctx: &RunCtx, reply: &Reply) -> Result<(), String> {
    match reply {
        Reply::ReleaseNotes(n) => {
            let body = format!(
                "# Release notes (run {})\n\n{}\n\n{}\n\nBreaking: {}\n",
                ctx.run_id,
                n.summary,
                n.changes
                    .iter()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                if n.breaking { "yes" } else { "no" }
            );
            std::fs::write(ctx.run_dir.join("release-notes.md"), body)
                .map_err(|e| format!("cannot write the release notes: {e}"))?;
            ctx.journal.append(
                "release_notes",
                json!({ "summary": n.summary, "changes": n.changes.len(), "breaking": n.breaking }),
            )?;
        }
        Reply::ReleasePlan(p) => {
            ctx.journal.append(
                "release_plan",
                json!({ "ready": p.ready, "summary": p.summary }),
            )?;
        }
        _ => {}
    }
    Ok(())
}
