//! `wg self gate --pr N` (design §12.3), run by the installed base binary, never the candidate. It
//! stops at the first failed step, appends every decision to the self-ledger and labels the PR:
//! `wg:self-held` with a comment naming the condition, or `wg:self-shadow` with "would merge" while
//! merging is off. Only after every step, and a re-read of the head, does it mint the
//! `SelfMergeDecision` that `gh pr merge --match-head-commit` needs.

use super::github::{self, SelfPr};
use super::ledger::Ledger;
use super::protected::Verdict;
use super::{diff, rate, state};
use crate::config::Config;
use crate::gh::Gh;
use crate::state_dirs::Dirs;
use crate::tokens::SelfMergeDecision;
use serde_json::{json, Value};
use std::path::Path;

/// Tier B (design §12.3 step 5): the base binary's live eval of the candidate's assets. `Err` means
/// the eval could not run (no sandbox, too few fixtures), so the PR is held, never merged.
pub trait LiveEval {
    fn run(&self, pr: u64, head: &str, overlay: &Path) -> Result<Report, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub passed: bool,
    pub reason: String,
    /// Spend per stage, arm and fixture, by provider, for the self-ledger.
    pub spend: Value,
}

pub struct Env<'a> {
    pub dirs: &'a Dirs,
    pub cfg: &'a Config,
    pub gh: &'a Gh,
    /// The owner's checkout of the factory repo: fetched, diffed, never checked out.
    pub checkout: &'a Path,
    pub merges_this_shift: u32,
    pub eval: &'a dyn LiveEval,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// CI still running, or a merge deferred by the rate limits: decided later, nothing recorded.
    Pending(String),
    Refused(String),
    /// Passed what it could, but the owner decides (editable Rust, eval unavailable, paused).
    Held(String),
    /// Would merge; merging is off (design §16 Phase 5).
    Shadow,
    Merged {
        merge: String,
    },
}

impl Outcome {
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Pending(_) => "pending",
            Outcome::Refused(_) => "refused",
            Outcome::Held(_) => "held",
            Outcome::Shadow => "shadow",
            Outcome::Merged { .. } => "merged",
        }
    }

    pub fn reason(&self) -> String {
        match self {
            Outcome::Pending(r) | Outcome::Refused(r) | Outcome::Held(r) => r.clone(),
            Outcome::Shadow => "would merge: every condition passed".into(),
            Outcome::Merged { merge } => format!("merged as {merge}"),
        }
    }
}

/// Step 1: label, base, factory authorship of every commit, state, mergeable, open count.
fn step1(p: &SelfPr, ledger: &Ledger, open: usize) -> Result<Option<Outcome>, String> {
    if !p.labels.iter().any(|l| l == github::SELF) || p.base_ref != "main" {
        return Ok(Some(Outcome::Refused(
            "not a self PR: it needs the wg:self label and base main".into(),
        )));
    }
    if p.state != "OPEN" || p.draft {
        return Ok(Some(Outcome::Refused(
            "the PR is not open and ready".into(),
        )));
    }
    if p.commits.is_empty() {
        return Ok(Some(Outcome::Refused("the PR has no commits".into())));
    }
    for (oid, msg) in &p.commits {
        let trailer = github::run_trailer(msg);
        let recorded = ledger.authored_by(oid)?;
        if trailer.is_none() || trailer != recorded {
            return Ok(Some(Outcome::Held(format!(
                "commit {} is not factory-produced (no Workgraph-Run trailer the self-ledger maps to it): the owner's PR now",
                &oid[..12]
            ))));
        }
    }
    if p.mergeable != "MERGEABLE" {
        return Ok(Some(Outcome::Held(format!(
            "the PR is not mergeable ({})",
            p.mergeable
        ))));
    }
    if open > super::config::MAX_OPEN_SELF_PRS {
        return Ok(Some(Outcome::Held(format!(
            "{open} self PRs are open, over the cap of {}",
            super::config::MAX_OPEN_SELF_PRS
        ))));
    }
    Ok(None)
}

/// Records the decision, labels and comments, and returns it.
fn finish(e: &Env, ledger: &Ledger, p: &SelfPr, base: &str, o: Outcome) -> Result<Outcome, String> {
    if matches!(o, Outcome::Pending(_)) {
        return Ok(o);
    }
    let st = state::load(&e.dirs.self_state())?;
    ledger.append(
        "decision",
        json!({ "pr": p.number, "head": p.head, "base": base, "outcome": o.name(),
                "reason": o.reason(), "shadow": !st.merge_enabled }),
    )?;
    let repo = &e.cfg.self_improve.repo;
    match &o {
        Outcome::Refused(why) | Outcome::Held(why) => {
            github::label_pr(e.gh, repo, p.number, github::HELD)?;
            github::comment_pr(
                e.gh,
                repo,
                p.number,
                &format!(
                    "workgraph self gate: {} at {}.\n\n```text\n{}\n```",
                    o.name(),
                    &p.head[..12],
                    why.replace("```", "'''")
                ),
            )?;
        }
        Outcome::Shadow => {
            github::label_pr(e.gh, repo, p.number, github::SHADOW)?;
            github::comment_pr(e.gh, repo, p.number, &format!(
                "workgraph self gate (shadow mode): would merge {} at {}. Merging is off until `wg self resume --enable-merge`; merge or close this by hand, and record your verdict.",
                p.number, &p.head[..12]
            ))?;
        }
        _ => {}
    }
    Ok(o)
}

pub fn gate(e: &Env, pr: u64) -> Result<Outcome, String> {
    let repo = e.cfg.self_improve.repo.clone();
    let ledger = Ledger::at(&e.dirs.self_ledger());
    ledger.entries()?;
    let p = github::self_pr(e.gh, &repo, pr)?;
    let open = github::open_self_prs(e.gh, &repo)?.len();
    if let Some(o) = step1(&p, &ledger, open)? {
        return finish(e, &ledger, &p, "", o);
    }
    // Step 2: the diff, computed here.
    diff::fetch(e.checkout, pr, &p.head)?;
    let rules = diff::rules(e.checkout)?;
    let d = diff::diff(e.checkout, &rules, &p.head)?;
    let base = d.base.clone();
    if let Verdict::Refused(why) = &d.verdict {
        return finish(
            e,
            &ledger,
            &p,
            &base,
            Outcome::Refused(format!("protected-paths: {}", why.join("; "))),
        );
    }
    let lint = diff::lint(e.checkout, &p.head, &d.changes)?;
    if !lint.is_empty() {
        return finish(
            e,
            &ledger,
            &p,
            &base,
            Outcome::Refused(format!("lint-editable: {}", lint.join("; "))),
        );
    }
    let assets = diff::asset_changes(&d.changes);
    let overlay_dir = e
        .dirs
        .eval_work()
        .join(format!("gate-{pr}-{}", &p.head[..12]));
    let overlay = diff::write_overlay(e.checkout, &p.head, &assets, &overlay_dir)?;
    if let Err(why) = diff::overlay_loads(&overlay, e.cfg.max_calls_per_task) {
        return finish(
            e,
            &ledger,
            &p,
            &base,
            Outcome::Refused(format!(
                "the candidate's prompts and pipelines do not load in the base kernel: {why}"
            )),
        );
    }
    // Step 3: required check-runs on H, from GitHub Actions only.
    let runs = github::check_runs(e.gh, &repo, &p.head)?;
    match github::required(&runs, &p.head, &rules.required_checks) {
        Err(running) => return finish(e, &ledger, &p, &base, Outcome::Pending(running)),
        Ok(Some(failed)) => return finish(e, &ledger, &p, &base, Outcome::Refused(failed)),
        Ok(None) => {}
    }
    if let Verdict::Held(files) = &d.verdict {
        return finish(e, &ledger, &p, &base, Outcome::Held(format!(
            "passed, held for the owner: editable Rust and other files outside auto_merge_paths (D2): {}",
            files.join(", ")
        )));
    }
    // Step 5: Tier B, only when prompts or pipelines changed.
    if !assets.is_empty() {
        let used = ledger
            .of_kind("eval")?
            .iter()
            .filter(|v| v["pr"] == pr)
            .count() as u32;
        if used >= e.cfg.self_improve.max_eval_runs_per_pr {
            return finish(
                e,
                &ledger,
                &p,
                &base,
                Outcome::Held(format!(
                    "eval unavailable: the PR used its {used} eval runs; a new push is the owner's"
                )),
            );
        }
        match e.eval.run(pr, &p.head, &overlay) {
            Err(why) => {
                return finish(
                    e,
                    &ledger,
                    &p,
                    &base,
                    Outcome::Held(format!("eval unavailable: {why}")),
                )
            }
            Ok(r) => {
                ledger.append(
                    "eval",
                    json!({ "pr": pr, "head": p.head, "passed": r.passed, "reason": r.reason, "spend": r.spend }),
                )?;
                if !r.passed {
                    return finish(
                        e,
                        &ledger,
                        &p,
                        &base,
                        Outcome::Refused(format!("eval: {}", r.reason)),
                    );
                }
            }
        }
    }
    // Step 6: rate and state; in shadow mode, stop here.
    let st = state::load(&e.dirs.self_state())?;
    if let Some(o) = rate::check(e, &ledger, &rules, &st)? {
        return finish(e, &ledger, &p, &base, o);
    }
    if !st.merge_enabled || rate::shadow_count(&ledger)? < rules.shadow_prs {
        return finish(e, &ledger, &p, &base, Outcome::Shadow);
    }
    // Step 7: the head must not have moved since it was judged.
    let again = github::self_pr(e.gh, &repo, pr)?;
    if again.head != p.head {
        return finish(
            e,
            &ledger,
            &p,
            &base,
            Outcome::Refused(format!(
                "the head moved from {} to {} after evaluation",
                &p.head[..12],
                &again.head[..12]
            )),
        );
    }
    let entry = ledger.append(
        "decision",
        json!({ "pr": pr, "head": p.head, "base": base, "outcome": "merging", "reason": "every condition passed", "shadow": false }),
    )?;
    let decision = SelfMergeDecision::new(pr, &p.head, &base, entry["seq"].as_u64().unwrap_or(0));
    e.gh.merge(&repo, &decision)?;
    let merge =
        e.gh.pr_view(&repo, pr)?
            .merge_commit
            .ok_or("the merged PR names no merge commit")?;
    ledger.append(
        "merged",
        json!({ "pr": pr, "head": p.head, "merge": merge, "decision": decision.ledger_entry() }),
    )?;
    Ok(Outcome::Merged { merge })
}
