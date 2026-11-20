//! Rollback (design §12.5), automatic on an unhealthy canary or by the owner with
//! `wg self rollback`: flip `current` to `previous`; restore `memory/` from the snapshot taken when
//! the bad binary was installed (the bad binary's memory is moved aside, never deleted); denylist
//! the SHA; open a revert PR from `wg-self/revert-<sha12>` that reverts the recorded merge SHA
//! (never auto-merged); pause the lane until the owner resumes it.

use super::install::{link, point};
use super::ledger::Ledger;
use super::state;
use crate::gh::Gh;
use crate::git::git;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::path::Path;

pub struct Env<'a> {
    pub dirs: &'a Dirs,
    pub gh: &'a Gh,
    pub repo: &'a str,
    /// The owner's checkout of the factory repo; the revert is made in a worktree off origin/main.
    pub checkout: Option<&'a Path>,
}

fn restore_memory(dirs: &Dirs, sha12: &str) -> Result<bool, String> {
    let snap = dirs.memory_snapshot(sha12);
    if !snap.exists() {
        return Ok(false);
    }
    let mem = dirs.memory_root();
    if mem.exists() {
        let aside = dirs
            .state()
            .join("memory-dropped")
            .join(crate::journal::now_nanos().to_string());
        crate::state_dirs::private_dir(aside.parent().unwrap_or(&aside))?;
        std::fs::rename(&mem, &aside).map_err(|e| format!("cannot move memory aside: {e}"))?;
    }
    crate::state_dirs::private_dir(&mem)?;
    for e in std::fs::read_dir(&snap).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        std::fs::copy(e.path(), mem.join(e.file_name()))
            .map_err(|e| format!("cannot restore memory: {e}"))?;
    }
    Ok(true)
}

/// Opens the revert PR for factory merge `sha`; returns its URL.
fn revert_pr(e: &Env, checkout: &Path, sha: &str, why: &str) -> Result<String, String> {
    let sha12 = &sha[..12];
    let branch = format!("wg-self/revert-{sha12}");
    let wt = e.dirs.eval_work().join(format!("revert-{sha12}"));
    crate::git::fetch(checkout)?;
    if wt.exists() {
        git(
            checkout,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
            &[],
        )?;
    }
    crate::state_dirs::private_dir(&e.dirs.eval_work())?;
    git(
        checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-B",
            &branch,
            &wt.to_string_lossy(),
            "origin/main",
        ],
        &[],
    )?;
    let made = (|| {
        git(&wt, &["revert", "--no-edit", sha], &[])?;
        crate::git::push_task_branch(&wt, &branch)?;
        e.gh.pr_create(
            e.repo,
            &branch,
            "main",
            &format!("revert: roll back self merge {sha12}"),
            &format!(
                "workgraph rolled back the binary built from {sha}: {why}\n\nThis reverts the self merge. It is never auto-merged: merge or close it yourself, then `wg self resume`."
            ),
            false,
            &[],
        )
    })();
    // Best effort: the worktree holds nothing the branch does not.
    let _ = git(
        checkout,
        &["worktree", "remove", "--force", &wt.to_string_lossy()],
        &[],
    );
    made
}

pub fn rollback(e: &Env, why: &str) -> Result<Value, String> {
    let dirs = e.dirs;
    let bad = link(dirs, "current").ok_or("nothing is installed")?;
    let good = link(dirs, "previous")
        .filter(|p| p != &bad)
        .ok_or("no previous binary to roll back to")?;
    point(dirs, "current", &good)?;
    // `previous` would name the bad binary's predecessor's predecessor; never leave it on the bad one.
    let _ = std::fs::remove_file(dirs.bin().join("previous"));
    let restored = restore_memory(dirs, good.trim_start_matches("wg-"))?;
    let ledger = Ledger::at(&dirs.self_ledger());
    let sha = ledger
        .of_kind("installed")?
        .iter()
        .rev()
        .find(|i| i["dir"] == bad.as_str())
        .and_then(|i| i["sha"].as_str().map(str::to_string));
    let st = state::load(&dirs.self_state())?;
    let st = match &sha {
        Some(s) => st.denied(s),
        None => st,
    };
    state::save(
        &dirs.self_state(),
        &st.paused_by(&format!("rolled back: {why}")),
    )?;
    let factory_merge = sha
        .as_deref()
        .map(|s| ledger.merges().map(|m| m.iter().any(|x| x["merge"] == s)))
        .transpose()?
        .unwrap_or(false);
    let revert = match (&sha, e.checkout, factory_merge) {
        (Some(s), Some(c), true) => Some(revert_pr(e, c, s, why)?),
        _ => None,
    };
    let out = json!({ "rolled_back": bad, "current": good, "sha": sha, "memory_restored": restored,
                      "revert_pr": revert, "paused": true, "why": why });
    ledger.append("rolled_back", out.clone())?;
    Ok(out)
}

/// After the first shift on a new binary: rolls back when the canary finds a problem.
pub fn after_shift(e: &Env, shift: &[Value], runs: &[Vec<Value>]) -> Result<Option<Value>, String> {
    let ledger = Ledger::at(&e.dirs.self_ledger());
    let Some(cur) = link(e.dirs, "current") else {
        return Ok(None);
    };
    let installed = ledger.of_kind("installed")?;
    let Some(last) = installed.iter().rev().find(|i| i["dir"] == cur.as_str()) else {
        return Ok(None);
    };
    let judged = ledger
        .of_kind("canary")?
        .iter()
        .any(|c| c["dir"] == cur.as_str() && c["installed_seq"] == last["seq"]);
    if judged {
        return Ok(None);
    }
    let problems = super::canary::problems(shift, runs);
    ledger.append(
        "canary",
        json!({ "dir": cur, "installed_seq": last["seq"], "healthy": problems.is_empty(), "problems": problems }),
    )?;
    if problems.is_empty() {
        return Ok(None);
    }
    rollback(e, &format!("unhealthy canary: {}", problems.join("; "))).map(Some)
}
