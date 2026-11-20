//! Every factory string that reaches an effect is validated here, and the kernel owns the
//! ordering: no Worker or Fixer before the baseline or while an escalation is open, no Reviewer
//! below opus, no Worker or Fixer for a unit the kernel holds risky (design §4.1, §4.2).

use super::kernel::Kernel;
use super::RunCtx;
use crate::claude::policy::{policy, Ctx as PolicyCtx};
use crate::claude::runner::Request;
use crate::guard::hook::SandboxGrant;
use crate::untrusted::Ctx as ParseCtx;
use wg_factory::api::{LlmCall, Role, Tier};

/// Why a command was refused, and whether the run must end `needs_decision` because of it.
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub why: String,
    pub needs_decision: bool,
}

fn refuse(why: impl Into<String>) -> Refusal {
    Refusal {
        why: why.into(),
        needs_decision: false,
    }
}

fn stage(role: Role) -> &'static str {
    match role {
        Role::Reader => "read",
        Role::Challenger => "challenge",
        Role::Worker => "work",
        Role::Fixer => "fix",
        Role::Reviewer => "review",
        Role::Learner => "learn",
        Role::QaExplorer => "qa",
        Role::ReleaseWriter => "release",
    }
}

/// The repo's `qa` check set (`[[repo]].qa_checks`).
pub fn qa_set(ctx: &RunCtx) -> Vec<String> {
    ctx.cfg
        .repo(&ctx.repo_name)
        .map(|r| r.qa_checks.clone())
        .unwrap_or_default()
}

/// Turns a factory call into the kernel's request, or refuses it.
pub fn llm(ctx: &RunCtx, k: &Kernel, c: &LlmCall, max_units: usize) -> Result<Request, Refusal> {
    crate::ids::label(&c.label).map_err(refuse)?;
    let template = ctx.assets.prompt(&c.template).ok_or_else(|| {
        refuse(format!(
            "unknown template {:?}",
            crate::ids::truncate(&c.template, 32)
        ))
    })?;
    let editing = matches!(c.role, Role::Worker | Role::Fixer);
    let mut unit_files: Vec<String> = Vec::new();
    let mut grant: Option<SandboxGrant> = None;
    if editing {
        let open = k.open();
        if !open.is_empty() {
            let ids: Vec<&str> = open.iter().map(|e| e.id.as_str()).collect();
            return Err(Refusal {
                why: format!("escalations without a decision: {}", ids.join(", ")),
                needs_decision: true,
            });
        }
        if !k.baseline_done {
            return Err(refuse("no edit may precede the kernel's baseline checks"));
        }
        let unit = c
            .unit
            .as_deref()
            .ok_or_else(|| refuse("a Worker or Fixer call names no unit"))?;
        unit_files = k
            .unit_files(unit)
            .ok_or_else(|| refuse(format!("unit {unit:?} is not in the kernel's Reading")))?;
        let reading_risky = k
            .reading
            .as_ref()
            .and_then(|r| r.units.iter().find(|u| u.id == unit))
            .is_some_and(|u| u.risky);
        if reading_risky || k.is_risky(unit) || k.tainted {
            if let Some(why) = k.sandbox_blocker(ctx) {
                return Err(refuse(format!(
                    "unit {unit} is risky (or the task is tainted) and runs only in the sandbox, which is unavailable: {why}"
                )));
            }
            grant = Some(SandboxGrant {
                repo: ctx.repo_name.clone(),
                run: ctx.run_id.clone(),
                unit: Some(unit.to_string()),
                task: None,
                files: unit_files.clone(),
            });
        }
    }
    if matches!(c.role, Role::Reviewer | Role::QaExplorer) && k.tainted {
        if let Some(why) = k.sandbox_blocker(ctx) {
            return Err(refuse(format!(
                "the task is tainted and its review commands run only in the sandbox, which is unavailable: {why}"
            )));
        }
        grant = Some(SandboxGrant {
            repo: ctx.repo_name.clone(),
            run: ctx.run_id.clone(),
            unit: None,
            task: Some(k.task_id()),
            files: vec![],
        });
    }
    if c.role == Role::Reviewer && c.tier != Tier::Opus {
        return Err(refuse("a Reviewer call must be opus"));
    }
    let task_id = k.task_id();
    let hx = format!("python3 {}", ctx.haruspex_cli);
    let prompt = crate::render::render(template, &c.vars, &[("hx", hx)]).map_err(refuse)?;
    let qa = if c.role == Role::QaExplorer {
        qa_set(ctx)
    } else {
        vec![]
    };
    let approved: Vec<String> = k
        .required
        .iter()
        .chain(qa.iter())
        .filter_map(|n| ctx.cfg.check(&ctx.repo_name, n))
        .flat_map(|e| [e.project.clone(), e.factory.clone()])
        .collect();
    let secret_roots = [".workgraph", ".ssh", ".config"]
        .iter()
        .map(|d| ctx.dirs.home.join(d).to_string_lossy().into_owned())
        .collect();
    let known_task = if task_id.is_empty() {
        k.resume_task.as_deref()
    } else {
        Some(task_id.as_str())
    };
    let p = policy(
        c.role,
        &unit_files,
        &PolicyCtx {
            worktree: &ctx.repo,
            run_tmp: &ctx.run_tmp,
            haruspex_cli: &ctx.haruspex_cli,
            task_id: known_task,
            initializing: k.initializing,
            approved: &approved,
            secret_roots,
            sandbox: grant.map(|g| (ctx.wg_exe.to_string_lossy().into_owned(), g)),
            edits_task: c.role == Role::ReleaseWriter
                && c.schema == wg_factory::api::SchemaId::ReleasePlan,
        },
    );
    let (turns, secs) = (
        c.max_turns.clamp(1, crate::config::MAX_TURNS),
        c.timeout_secs.max(1),
    );
    Ok(Request {
        label: c.label.clone(),
        role: c.role,
        stage: stage(c.role),
        tier: c.tier,
        effort: c.effort,
        prompt,
        schema: c.schema,
        max_turns: turns,
        timeout_secs: secs.min(1800),
        policy: p,
        parse: ParseCtx {
            max_units,
            start_branch: k.start_branch.clone(),
            known_criteria: k.known_criteria(),
            fixed_sigs: k.seen_sigs.clone(),
        },
        task: task_id,
    })
}

/// Check names must be known; the kernel then runs its own set, whatever the factory listed: the
/// gate, plus the `qa` checks the factory named, plus the chain's whole QA set once it has run (a
/// fix after `regression` passed is checked against it again, design §6.3).
pub fn checks(
    k: &Kernel,
    label: &str,
    names: &[String],
    qa: &[String],
) -> Result<Vec<String>, Refusal> {
    crate::ids::label(label).map_err(refuse)?;
    if let Some(n) = names
        .iter()
        .find(|n| !k.required.contains(n) && !qa.contains(n))
    {
        return Err(refuse(format!(
            "unknown check {:?}",
            crate::ids::truncate(n, 32)
        )));
    }
    if label == "check:baseline" {
        return Ok(k.required.clone());
    }
    if !k.baseline_done {
        return Err(refuse("the baseline must run first"));
    }
    let started = if k.qa_started() { &k.qa[..] } else { &[] };
    let mut out = k.gate.clone();
    for n in names.iter().chain(started) {
        if qa.contains(n) && !out.contains(n) {
            out.push(n.clone());
        }
    }
    Ok(out)
}

/// A `Gate` command: a known gate, evaluated `--json` only.
pub fn gate(label: &str, gate: &str) -> Result<(), Refusal> {
    crate::ids::label(label).map_err(refuse)?;
    if !crate::pipelines::GATES.contains(&gate) {
        return Err(refuse(format!(
            "unknown gate {:?}",
            crate::ids::truncate(gate, 32)
        )));
    }
    Ok(())
}

/// A `Sandbox` command: a known step set, configured for this repo; its commands, in order.
pub fn sandbox_steps(ctx: &RunCtx, label: &str, steps_from: &str) -> Result<Vec<String>, Refusal> {
    crate::ids::label(label).map_err(refuse)?;
    if !crate::pipelines::STEP_SETS.contains(&steps_from) {
        return Err(refuse(format!(
            "unknown step set {:?}",
            crate::ids::truncate(steps_from, 32)
        )));
    }
    let steps = ctx
        .cfg
        .repo(&ctx.repo_name)
        .and_then(|r| r.sandbox.as_ref())
        .map(|s| s.release_rehearsal.clone())
        .unwrap_or_default();
    if steps.is_empty() {
        return Err(refuse(format!(
            "[repo.sandbox].{steps_from} has no steps for {}",
            ctx.repo_name
        )));
    }
    Ok(steps)
}
