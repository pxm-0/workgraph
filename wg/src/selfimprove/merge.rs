//! Gate step 7's merge (design §12.3) and its record. The `merging` decision is in the self-ledger
//! before `gh pr merge` runs, and counts as a merge for every rate limit until a `merged` entry
//! names its merge SHA (see `Ledger::merges`). A merge call that fails is judged by GitHub's own
//! state of the PR, never by the exit code: a retried merge can fail after the first try landed.
//! A merge SHA GitHub has not reported yet is recorded by `reconcile` on a later gate run.

use super::gate::Outcome;
use super::ledger::Ledger;
use crate::gh::Gh;
use crate::tokens::SelfMergeDecision;
use serde_json::json;
use std::time::Duration;

/// Records the merge SHA of `pr` if GitHub reports one; true when it did.
fn record(
    gh: &Gh,
    repo: &str,
    ledger: &Ledger,
    pr: u64,
    head: &str,
    seq: u64,
) -> Result<bool, String> {
    let Some(merge) = gh.pr_view(repo, pr)?.merge_commit else {
        return Ok(false);
    };
    ledger.append(
        "merged",
        json!({ "pr": pr, "head": head, "merge": merge, "decision": seq }),
    )?;
    Ok(true)
}

/// Fills in the merge SHA of every earlier merge GitHub did not name at the time.
pub fn reconcile(gh: &Gh, repo: &str, ledger: &Ledger) -> Result<(), String> {
    for (pr, head, seq) in ledger.unresolved_merges()? {
        record(gh, repo, ledger, pr, &head, seq)?;
    }
    Ok(())
}

/// What a `merging` decision came to. `Ok(None)`: the merge did not happen (the caller records
/// the refusal); `Err`: it happened, but its merge SHA is not known yet.
pub fn merge(
    gh: &Gh,
    repo: &str,
    ledger: &Ledger,
    decision: &SelfMergeDecision,
) -> Result<Result<Outcome, String>, String> {
    let (pr, head, seq) = (decision.pr(), decision.head(), decision.ledger_entry());
    if let Err(why) = gh.merge(repo, decision) {
        if gh.pr_view(repo, pr)?.state != "MERGED" {
            return Ok(Err(format!("the merge failed: {why}")));
        }
    }
    let waits = std::iter::once(Duration::ZERO).chain(gh.delays.iter().copied());
    for wait in waits {
        std::thread::sleep(wait);
        if record(gh, repo, ledger, pr, head, seq)? {
            let m = ledger.merges()?;
            let sha = m
                .iter()
                .rev()
                .find(|x| x["pr"] == pr)
                .and_then(|x| x["merge"].as_str())
                .unwrap_or_default();
            return Ok(Ok(Outcome::Merged { merge: sha.into() }));
        }
    }
    Err(format!(
        "#{pr} merged, but GitHub names no merge commit yet; the next gate run records it"
    ))
}
