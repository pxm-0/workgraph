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

/// Opens the revert PR for factory merge `sha` from a worktree under `worktrees/` (design §14
/// checklist 2); returns its URL. A failed cleanup is returned beside it, never dropped.
fn revert_pr(
    e: &Env,
    checkout: &Path,
    sha: &str,
    why: &str,
) -> Result<(String, Option<String>), String> {
    let sha12 = &sha[..12];
    let branch = format!("wg-self/revert-{sha12}");
    let root = e.dirs.worktrees();
    let wt = root.join(format!("revert-{sha12}"));
    crate::git::fetch(checkout)?;
    if wt.exists() {
        crate::git::remove_worktree(checkout, &root, &wt)?;
    }
    crate::state_dirs::private_dir(&root)?;
    // A retry after the push but before the PR reuses the pushed revert.
    let remote = format!("refs/remotes/origin/{branch}");
    let pushed = git(checkout, &["rev-parse", "--verify", "-q", &remote], &[]).is_ok();
    let from = if pushed {
        remote.as_str()
    } else {
        "origin/main"
    };
    git(
        checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-B",
            &branch,
            &wt.to_string_lossy(),
            from,
        ],
        &[],
    )?;
    let made = (|| {
        if !pushed {
            git(&wt, &["revert", "--no-edit", sha], &[])?;
            crate::git::push_task_branch(&wt, &branch)?;
        }
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
    let cleanup = crate::git::remove_worktree(checkout, &root, &wt).err();
    made.map(|url| (url, cleanup))
}

/// The SHA of the last rollback whose revert PR is still owed.
fn pending_revert(ledger: &Ledger) -> Result<Option<String>, String> {
    let all = ledger.entries()?;
    Ok(all
        .iter()
        .rev()
        .find(|r| r["kind"] == "rolled_back" && r["revert_owed"] == true)
        .and_then(|r| r["sha"].as_str())
        .filter(|s| {
            !all.iter()
                .any(|o| o["kind"] == "revert_opened" && o["sha"] == *s)
        })
        .map(str::to_string))
}

/// Opens the owed revert PR and records the result: `revert_opened`, or `revert_failed` with the
/// error, which the next `wg self rollback` retries.
fn open_revert(e: &Env, ledger: &Ledger, sha: &str, why: &str) -> Result<Value, String> {
    let Some(c) = e.checkout else {
        let error = "no checkout of the factory repo is configured";
        ledger.append("revert_failed", json!({ "sha": sha, "error": error }))?;
        return Ok(json!({ "sha": sha, "revert_pr": null, "revert_error": error }));
    };
    match revert_pr(e, c, sha, why) {
        Ok((url, cleanup)) => {
            ledger.append(
                "revert_opened",
                json!({ "sha": sha, "url": url, "cleanup_error": cleanup }),
            )?;
            Ok(json!({ "sha": sha, "revert_pr": url, "cleanup_error": cleanup }))
        }
        Err(error) => {
            ledger.append("revert_failed", json!({ "sha": sha, "error": error }))?;
            Ok(json!({ "sha": sha, "revert_pr": null, "revert_error": error }))
        }
    }
}

/// Flips `current` back, then records the rollback before anything that can fail after the flip;
/// a revert PR that could not open is retried by the next call.
pub fn rollback(e: &Env, why: &str) -> Result<Value, String> {
    let dirs = e.dirs;
    let ledger = Ledger::at(&dirs.self_ledger());
    let bad = link(dirs, "current").ok_or("nothing is installed")?;
    let Some(good) = link(dirs, "previous").filter(|p| p != &bad) else {
        return match pending_revert(&ledger)? {
            Some(sha) => open_revert(e, &ledger, &sha, why),
            None => Err("no previous binary to roll back to".into()),
        };
    };
    let sha = ledger
        .of_kind("installed")?
        .iter()
        .rev()
        .find(|i| i["dir"] == bad.as_str())
        .and_then(|i| i["sha"].as_str().map(str::to_string));
    state::update(&dirs.self_state(), |st| {
        let st = match &sha {
            Some(s) => st.denied(s),
            None => st.clone(),
        };
        Ok(st.paused_by(&format!("rolled back: {why}")))
    })?;
    point(dirs, "current", &good)?;
    // `previous` would name the bad binary's predecessor's predecessor; never leave it on the bad one.
    let unlinked = std::fs::remove_file(dirs.bin().join("previous"))
        .err()
        .map(|e| format!("cannot remove bin/previous: {e}"));
    let (restored, memory_error) = match restore_memory(dirs, good.trim_start_matches("wg-")) {
        Ok(r) => (r, None),
        Err(e) => (false, Some(e)),
    };
    let factory_merge = sha
        .as_deref()
        .map(|s| ledger.merges().map(|m| m.iter().any(|x| x["merge"] == s)))
        .transpose()?
        .unwrap_or(false);
    let mut out = json!({ "rolled_back": bad, "current": good, "sha": sha, "memory_restored": restored,
                          "memory_error": memory_error, "unlink_error": unlinked,
                          "revert_owed": factory_merge, "revert_pr": null, "paused": true, "why": why });
    ledger.append("rolled_back", out.clone())?;
    if let (Some(s), true) = (&sha, factory_merge) {
        let r = open_revert(e, &ledger, s, why)?;
        out["revert_pr"] = r["revert_pr"].clone();
        for k in ["revert_error", "cleanup_error"] {
            if !r[k].is_null() {
                out[k] = r[k].clone();
            }
        }
    }
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
    // Only a shift that ran this binary judges it.
    let bin_hash = crate::haruspex::cli::sha256_file(&e.dirs.bin().join(&cur).join("wg"))?;
    let ran = shift
        .iter()
        .find(|x| x["kind"] == "shift_started")
        .and_then(|x| x["binary"].as_str());
    // A shift that ended before its work (window closed, preflight, sandbox) judges nothing.
    let early = shift.iter().any(|x| x["kind"] == "shift_ended_early");
    if ran != Some(&bin_hash[..12]) || early {
        return Ok(None);
    }
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
