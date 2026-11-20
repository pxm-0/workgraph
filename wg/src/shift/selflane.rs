//! The self lane of a shift (design §7.3 step 11, §12.1): factory proposals filed as
//! `wg:self-proposed` issues (never taken), at most one owner-queued `wg:self` task within the
//! day and week caps and the open-PR cap, the head it published recorded in the self-ledger, then
//! the self-merge gate for every open self PR whose CI finished. There is no dollar cap (D3).

use super::config::{RepoCfg, Shift};
use super::intake;
use super::task::{Env, Kind};
use crate::config::Config;
use crate::journal::now;
use crate::selfimprove::config::{MAX_OPEN_SELF_PRS, MAX_PROPOSALS_PER_SHIFT};
use crate::selfimprove::github::{self, PROPOSED, SELF};
use crate::selfimprove::ledger::Ledger;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The shift settings for `repo`'s intake: the self lane queues on `wg:self`.
pub fn lane_shift(cfg: &Config, repo: &RepoCfg) -> Shift {
    if repo.is_self() {
        Shift {
            label: SELF.into(),
            ..cfg.shift.clone()
        }
    } else {
        cfg.shift.clone()
    }
}

/// Why no self task may start now: the day and week caps on self PRs, or too many open.
pub fn caps(cfg: &Config, ledger: &Ledger, open: usize) -> Result<Option<String>, String> {
    let authored = ledger.of_kind("authored")?;
    let since = |secs: u64| {
        authored
            .iter()
            .filter(|a| a["at"].as_u64().unwrap_or(0) + secs > now())
            .count() as u32
    };
    let s = &cfg.self_improve;
    Ok(if since(86_400) >= s.max_self_prs_per_day {
        Some(format!(
            "max_self_prs_per_day ({}) reached",
            s.max_self_prs_per_day
        ))
    } else if since(7 * 86_400) >= s.max_self_prs_per_week {
        Some(format!(
            "max_self_prs_per_week ({}) reached",
            s.max_self_prs_per_week
        ))
    } else if open >= MAX_OPEN_SELF_PRS {
        Some(format!(
            "{open} self PRs are open (cap {MAX_OPEN_SELF_PRS})"
        ))
    } else {
        None
    })
}

/// Files this shift's `factory` proposals (design §10.4): at most two, deduplicated by text hash,
/// each recorded in the self-ledger and the intake ledger so no intake ever takes it.
pub fn file_proposals(e: &Env) -> Result<Vec<u64>, String> {
    let repo = &e.cfg.self_improve.repo;
    let ledger = Ledger::at(&e.dirs.self_ledger());
    let mut seen: Vec<String> = ledger
        .of_kind("proposal_filed")?
        .iter()
        .filter_map(|p| p["hash"].as_str().map(str::to_string))
        .collect();
    let events = crate::journal::read(e.journal.path())?;
    let mut filed = Vec::new();
    for ev in events.iter().filter(|v| v["kind"] == "proposals") {
        for p in ev["proposals"].as_array().into_iter().flatten() {
            if p["category"] != "factory" || filed.len() >= MAX_PROPOSALS_PER_SHIFT {
                continue;
            }
            let text = p["proposal"].as_str().unwrap_or("").trim().to_string();
            let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
            if text.is_empty() || seen.contains(&hash) {
                continue;
            }
            if filed.is_empty() {
                e.gh.run(&[
                    "label",
                    "create",
                    PROPOSED,
                    "-R",
                    repo,
                    "--color",
                    "C5DEF5",
                    "--description",
                    "workgraph: a factory proposal, never worked unless the owner files it",
                    "--force",
                ])?;
            }
            let body = format!(
                "A factory proposal from the learn step of {}#{} (shift `{}`). It derives from untrusted repo evidence; it is shown, never applied, and the self lane never takes this issue. To act on it, file your own issue and label it `wg:self`.\n\n```text\n{}\n```\n\nEvidence:\n\n```text\n{}\n```",
                ev["repo"].as_str().unwrap_or(""), ev["issue"], e.shift_id,
                text.replace("```", "'''"),
                p["evidence"].as_str().unwrap_or("").replace("```", "'''")
            );
            let n = github::create_issue(
                e.gh,
                repo,
                &format!("factory: {}", crate::ids::one_line(&text, 80)),
                &body,
                PROPOSED,
            )?;
            intake::ledger(
                e.dirs,
                json!({ "kind": "created", "repo": repo, "issue": n, "at": now() }),
            )?;
            ledger.append("proposal_filed", json!({ "repo": repo, "issue": n, "hash": hash, "source": ev["repo"], "source_issue": ev["issue"] }))?;
            e.journal
                .append("proposal_filed", json!({ "repo": repo, "issue": n }))?;
            seen.push(hash);
            filed.push(n);
        }
    }
    Ok(filed)
}

/// At most one `wg:self` task, then the gate for every open self PR. `stopped`: the shift is
/// stopping (no new task; the gate still runs, it starts no agent).
pub fn lane(e: &Env, repo: &RepoCfg, stopped: bool) -> Result<Value, String> {
    let ledger = Ledger::at(&e.dirs.self_ledger());
    ledger.entries()?;
    let filed = file_proposals(e)?;
    let st = crate::selfimprove::state::load(&e.dirs.self_state())?;
    let open = github::open_self_prs(e.gh, &repo.name)?;
    let why_not = if stopped {
        Some("the shift is stopping".to_string())
    } else if st.paused {
        Some(format!(
            "the lane is paused: {}",
            st.pause_reason.as_deref().unwrap_or("")
        ))
    } else {
        caps(e.cfg, &ledger, open.len())?
    };
    let mut task = json!(null);
    match why_not {
        Some(w) => {
            e.journal
                .append("self_lane", json!({ "task": null, "why": w }))?;
        }
        None => task = one_task(e, repo)?,
    }
    let mut gated = Vec::new();
    let mut merges = 0;
    for pr in github::open_self_prs(e.gh, &repo.name)? {
        let before = ledger.merges()?.len();
        let o = crate::cli::self_::run_gate(e.dirs, e.cfg, e.gh, pr, merges);
        let (name, reason) = match &o {
            Ok(o) => (o.name().to_string(), o.reason()),
            Err(why) => ("error".to_string(), why.clone()),
        };
        // The ledger, not the outcome, says whether a merge happened: an error after the merge
        // still counts toward one merge per shift.
        if ledger.merges()?.len() > before {
            merges += 1;
        }
        e.journal.append(
            "self_gate",
            json!({ "pr": pr, "outcome": name, "reason": reason }),
        )?;
        gated.push(json!({ "pr": pr, "outcome": name }));
    }
    Ok(json!({ "proposals": filed, "task": task, "gated": gated }))
}

/// Takes the first owner-queued `wg:self` issue no factory process labelled, works it, and
/// records the published head in the self-ledger.
fn one_task(e: &Env, repo: &RepoCfg) -> Result<Value, String> {
    let shift = lane_shift(e.cfg, repo);
    let reruns = super::record::issues_of(e.dirs, &repo.name)?;
    let picked = intake::intake(
        e.gh,
        &shift,
        &repo.name,
        &intake::created(e.dirs, &repo.name),
        &reruns,
    )?;
    for (n, why) in &picked.skipped {
        e.journal.append(
            "issue_skipped",
            json!({ "repo": repo.name, "issue": n, "why": why }),
        )?;
    }
    let Some(i) = picked
        .picked
        .iter()
        .find(|i| !intake::factory_labelled(e.dirs, &repo.name, i.number, SELF))
    else {
        e.journal.append(
            "self_lane",
            json!({ "task": null, "why": "no wg:self issue queued" }),
        )?;
        return Ok(json!(null));
    };
    if let Some(why) = intake::still_queued(e.gh, &shift, &repo.name, i) {
        e.journal.append(
            "issue_skipped",
            json!({ "repo": repo.name, "issue": i.number, "why": why }),
        )?;
        return Ok(json!(null));
    }
    let d = super::run::work_one(e, repo, i, false)?;
    let mut out = json!({ "issue": i.number, "outcome": d.kind.name() });
    if let Some(pr) = authored(e, repo, i.number, &d)? {
        out["pr"] = json!(pr);
    }
    e.journal.append("self_lane", json!({ "task": out }))?;
    Ok(out)
}

/// Records the head a self task published, with its run and spend, so the gate knows the factory
/// authored it and the day and week caps count it. Called after a shift's task and after
/// `wg shift resume` finishes one; the PR number when there was a PR.
pub fn authored(
    e: &Env,
    repo: &RepoCfg,
    issue: u64,
    d: &super::task::Done,
) -> Result<Option<u64>, String> {
    if !repo.is_self() || d.kind != Kind::Pr {
        return Ok(None);
    }
    let rec = super::record::load(e.dirs, &repo.name, issue)?.ok_or("no task record")?;
    let head = crate::git::head(std::path::Path::new(&rec.worktree))?;
    let pr = rec.pr.ok_or("the self PR has no number")?;
    let spend = run_spend(e, d.run.as_deref().unwrap_or(""))?;
    Ledger::at(&e.dirs.self_ledger()).append(
        "authored",
        json!({ "repo": repo.name, "pr": pr, "head": head, "run": d.run, "branch": rec.branch, "issue": issue, "spend": spend }),
    )?;
    Ok(Some(pr))
}

/// A run's spend by provider, from the cost ledger (micro-dollars; unknown costs counted apart).
fn run_spend(e: &Env, run: &str) -> Result<Value, String> {
    let lines = crate::journal::read(&e.dirs.ledger()).unwrap_or_default();
    let mut by: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut unknown = 0;
    for l in lines.iter().filter(|l| l["run"] == run) {
        match l["cost_micros"].as_u64() {
            Some(c) => {
                let p = l["provider"].as_str().unwrap_or("unknown").to_string();
                let now = by.get(&p).and_then(Value::as_u64).unwrap_or(0);
                by.insert(p, json!(now + c));
            }
            None => unknown += 1,
        }
    }
    Ok(json!({ "by_provider_micros": by, "calls_with_unknown_cost": unknown }))
}
