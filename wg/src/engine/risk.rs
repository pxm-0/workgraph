//! The kernel's risk view as the run goes (design §8.1-§8.2): a risky unit being built taints the
//! task, and after each layer the classifier reads the actual diff, so a unit that turns out risky
//! has its checks re-run in the sandbox, or the task parks when there is none. Never the Mac.

use super::driver::End;
use super::kernel::Kernel;
use super::RunCtx;
use crate::sandbox::patch::in_unit;
use serde_json::json;
use wg_factory::api::{RiskView, Role};

/// In a shift, for a repo that has a sandbox, risky work that cannot run there parks the whole
/// task back to the queue instead of shipping the rest without it (design §7.3 step 8). In place,
/// or for a repo with no `[repo.sandbox]`, the factory skips risky units as before.
pub fn park_before_build(ctx: &RunCtx, k: &Kernel, view: &RiskView) -> Option<String> {
    let has_sandbox = ctx
        .cfg
        .repo(&ctx.repo_name)
        .is_some_and(|r| r.sandbox.is_some());
    if ctx.shift.is_none() || !has_sandbox || view.sandbox_ready {
        return None;
    }
    if view.risky_units.is_empty() && !view.tainted {
        return None;
    }
    let why = k.sandbox_blocker(ctx).unwrap_or_else(|| "not ready".into());
    Some(format!("sandbox unavailable: {why}"))
}

/// Records that a Worker or Fixer call for `unit` was accepted; true when it newly taints the task.
pub fn taints(k: &mut Kernel, role: Role, unit: &str) -> bool {
    if !matches!(role, Role::Worker | Role::Fixer) || !k.is_risky(unit) {
        return false;
    }
    if !k.built_risky.iter().any(|u| u == unit) {
        k.built_risky.push(unit.to_string());
    }
    let newly = !k.tainted;
    k.tainted = true;
    newly
}

/// The classifier on what each not-yet-risky unit actually changed: its paths, then the lines it
/// added. A hit taints the task; without a usable sandbox the task parks. A risky unit whose files
/// changed though no Worker of this run built it (a kept worktree's leftovers) taints it too.
pub fn after_layer(ctx: &RunCtx, k: &mut Kernel) -> Result<Option<End>, String> {
    let Some(reading) = k.reading.clone() else {
        return Ok(None);
    };
    let changed = ctx.ports.repo.changed()?;
    let mut found = Vec::new();
    let mut leftovers = Vec::new();
    for u in &reading.units {
        let touched: Vec<String> = changed
            .iter()
            .filter(|f| in_unit(f, &u.files))
            .cloned()
            .collect();
        if touched.is_empty() {
            continue;
        }
        if k.is_risky(&u.id) {
            let owners = touched.iter().any(|f| !k.user_changes.contains(f));
            if owners && !k.built_risky.contains(&u.id) {
                leftovers.push(u.id.clone());
            }
            continue;
        }
        let why = match crate::risk::paths(&touched) {
            Some(w) => Some(w),
            None => crate::risk::diff(&ctx.ports.repo.diff(&touched)?, &u.id),
        };
        if let Some(w) = why {
            found.push((u.id.clone(), w));
        }
    }
    if found.is_empty() && leftovers.is_empty() {
        return Ok(None);
    }
    k.built_risky.extend(leftovers.iter().cloned());
    k.built_risky.extend(found.iter().map(|(id, _)| id.clone()));
    k.risky.extend(found.iter().cloned());
    k.tainted = true;
    ctx.journal.append(
        "risk_updated",
        json!({ "after_the_fact": found, "leftovers": leftovers, "tainted": true }),
    )?;
    Ok(k.sandbox_blocker(ctx).map(|why| {
        let ids: Vec<&str> = found
            .iter()
            .map(|(id, _)| id.as_str())
            .chain(leftovers.iter().map(String::as_str))
            .collect();
        End::Parked(format!(
            "sandbox unavailable: {} turned out risky after it was built, and its checks may not run on the Mac: {why}",
            ids.join(", ")
        ))
    }))
}
