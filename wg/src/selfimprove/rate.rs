//! Gate step 6 (design §12.3): the lane is enabled and not paused, no STOP file, one merge per
//! shift, `min_hours_between_merges` since the last, below the drift checkpoint, and no owner
//! revert of a self PR since the last resume. A revert or the checkpoint pauses the lane.

use super::gate::{Env, Outcome};
use super::ledger::Ledger;
use super::protected::Rules;
use super::state::{self, SelfState};
use serde_json::Value;
use std::path::Path;

/// Distinct PRs the gate decided while merging was off.
pub fn shadow_count(ledger: &Ledger) -> Result<u32, String> {
    let mut prs: Vec<u64> = ledger
        .of_kind("decision")?
        .iter()
        .filter(|d| d["shadow"] == true && d["outcome"] != "pending")
        .filter_map(|d| d["pr"].as_u64())
        .collect();
    prs.sort_unstable();
    prs.dedup();
    Ok(prs.len() as u32)
}

/// Revert commits on origin/main since `since` (committer time) that name a recorded merge SHA,
/// as `(revert sha, reverted merge)`. Squash merges get a new SHA on main, so the ledger keeps it.
pub fn owner_reverts(
    repo: &Path,
    merges: &[Value],
    since: u64,
) -> Result<Vec<(String, String)>, String> {
    let log = crate::git::git(
        repo,
        &["log", "origin/main", "--format=%H%x1f%ct%x1f%B%x1e"],
        &[],
    )?;
    let mut out = Vec::new();
    for c in log.split('\x1e') {
        let mut f = c.trim_start_matches('\n').splitn(3, '\x1f');
        let (sha, ct, body) = (
            f.next().unwrap_or(""),
            f.next().unwrap_or(""),
            f.next().unwrap_or(""),
        );
        if ct.trim().parse::<u64>().unwrap_or(0) <= since {
            continue;
        }
        for m in merges.iter().filter_map(|m| m["merge"].as_str()) {
            if body.contains(&format!("This reverts commit {m}")) {
                out.push((sha.to_string(), m.to_string()));
            }
        }
    }
    Ok(out)
}

fn pause(e: &Env, st: &SelfState, why: &str) -> Result<Outcome, String> {
    state::save(&e.dirs.self_state(), &st.paused_by(why))?;
    Ok(Outcome::Held(format!("the lane is now paused: {why}")))
}

pub fn check(
    e: &Env,
    ledger: &Ledger,
    rules: &Rules,
    st: &SelfState,
) -> Result<Option<Outcome>, String> {
    let merges = ledger.merges()?;
    let reverts = owner_reverts(e.checkout, &merges, st.resumed_at)?;
    if let Some((rev, merge)) = reverts.first() {
        return pause(
            e,
            st,
            &format!("the owner reverted self merge {merge} in {rev}"),
        )
        .map(Some);
    }
    if !e.cfg.self_improve.enabled {
        return Ok(Some(Outcome::Held(
            "[self_improve].enabled is false".into(),
        )));
    }
    if st.paused {
        return Ok(Some(Outcome::Held(format!(
            "the lane is paused ({}); `wg self resume` is the owner's",
            st.pause_reason.as_deref().unwrap_or("no reason recorded")
        ))));
    }
    if e.dirs.stop_file().exists() {
        return Ok(Some(Outcome::Held("the STOP file exists".into())));
    }
    if e.merges_this_shift >= 1 {
        return Ok(Some(Outcome::Pending("one self merge per shift".into())));
    }
    let now = crate::journal::now();
    if let Some(last) = merges.iter().filter_map(|m| m["at"].as_u64()).max() {
        let next = last + u64::from(rules.min_hours_between_merges) * 3600;
        if now < next {
            return Ok(Some(Outcome::Pending(format!(
                "{} h between merges: the next is allowed {} s from now",
                rules.min_hours_between_merges,
                next - now
            ))));
        }
    }
    let since_resume = merges
        .iter()
        .filter(|m| m["at"].as_u64().unwrap_or(0) > st.resumed_at)
        .count() as u32;
    if since_resume >= rules.drift_checkpoint_every {
        return pause(
            e,
            st,
            &format!("drift checkpoint: {since_resume} self merges since the last resume"),
        )
        .map(Some);
    }
    Ok(None)
}
