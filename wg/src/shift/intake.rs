//! Intake (design §7.3 steps 4-5): which queued issues of a repo are work, in what order, and the
//! claim. Every skipped issue keeps its reason for the report.

use crate::gh::Gh;
use crate::shift::config::Shift;
use crate::state_dirs::Dirs;
use crate::untrusted::issue::{self, Issue};
use serde_json::{json, Value};
use std::io::Write;

pub const IN_SHIFT: &str = "wg:in-shift";
pub const P1: &str = "wg:p1";

#[derive(Debug, Default)]
pub struct Intake {
    pub picked: Vec<Issue>,
    pub skipped: Vec<(u64, String)>,
}

/// Appends one entry to the intake ledger.
pub fn ledger(dirs: &Dirs, entry: Value) -> Result<(), String> {
    let p = dirs.intake_ledger();
    if let Some(d) = p.parent() {
        crate::state_dirs::private_dir(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|e| format!("cannot open {}: {e}", p.display()))?;
    writeln!(f, "{entry}").map_err(|e| format!("cannot write {}: {e}", p.display()))
}

/// Issues of `repo` the factory created; intake never takes them.
pub fn created(dirs: &Dirs, repo: &str) -> Vec<u64> {
    std::fs::read_to_string(dirs.intake_ledger())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["kind"] == "created" && v["repo"] == repo)
        .filter_map(|v| v["issue"].as_u64())
        .collect()
}

/// True when the factory itself ever put `label` on issue `n` of `repo` (the intake ledger of its
/// own label edits): such a label is never the owner's queueing (design §7.3 step 11).
pub fn factory_labelled(dirs: &Dirs, repo: &str, n: u64, label: &str) -> bool {
    std::fs::read_to_string(dirs.intake_ledger())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|v| {
            v["kind"] == "label"
                && v["repo"] == repo
                && v["issue"] == n
                && v["add"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == label))
        })
}

/// A `wg:*` state label other than the queue label and the priority label.
pub fn other_state(i: &Issue, queue: &str) -> Option<String> {
    i.labels
        .iter()
        .find(|l| l.starts_with("wg:") && l.as_str() != queue && l.as_str() != P1)
        .cloned()
}

/// The queued issues of `repo` that are work: `wg:p1` first, then oldest. `reruns` are the issues
/// with a task record, checked for untrusted edits back to their first queue label.
pub fn intake(
    gh: &Gh,
    s: &Shift,
    repo: &str,
    created: &[u64],
    reruns: &[u64],
) -> Result<Intake, String> {
    let (issues, bad) = gh.issues(repo, &s.label)?;
    let mut out = Intake {
        skipped: bad.into_iter().map(|why| (0, why)).collect(),
        ..Intake::default()
    };
    for i in issues {
        let why = if created.contains(&i.number) {
            Some("created by the factory".to_string())
        } else if let Some(l) = other_state(&i, &s.label) {
            Some(format!("already carries {l}"))
        } else if !s.trusted_actors.contains(&i.author) {
            Some(format!(
                "written by {}, who is not a trusted actor",
                i.author
            ))
        } else {
            // A GitHub failure on one issue skips that issue, not the repo.
            gh.timeline(repo, i.number)
                .and_then(|(ev, renames)| {
                    let edits = gh.body_edits(repo, i.number)?;
                    let all: Vec<_> = edits.into_iter().chain(renames).collect();
                    let first = reruns.contains(&i.number);
                    issue::trusted(&i, &s.label, &ev, &all, &s.trusted_actors, first).map(|_| ())
                })
                .err()
        };
        match why {
            Some(w) => out.skipped.push((i.number, w)),
            None => out.picked.push(i),
        }
    }
    out.picked.sort_by_key(|i| {
        (
            !i.labels.iter().any(|l| l == P1),
            i.created_at.clone(),
            i.number,
        )
    });
    Ok(out)
}

/// Why a picked issue is no longer work just before its claim (intake may have run hours ago):
/// closed, no longer queued, another state label, or its title or body changed since intake.
pub fn still_queued(gh: &Gh, s: &Shift, repo: &str, picked: &Issue) -> Option<String> {
    let now = match gh.issue(repo, picked.number) {
        Ok(i) => i,
        Err(e) => return Some(format!("cannot read it again before the claim: {e}")),
    };
    if now.state != "OPEN" {
        return Some("closed before its claim".into());
    }
    if !now.labels.contains(&s.label) {
        return Some(format!("no longer carries {}", s.label));
    }
    if let Some(l) = other_state(&now, &s.label) {
        return Some(format!("now carries {l}"));
    }
    (issue::body_sha256(&now) != issue::body_sha256(picked))
        .then(|| "its title or body changed after intake".to_string())
}

/// Absolute label edits, recorded in the intake ledger.
pub fn relabel(
    dirs: &Dirs,
    gh: &Gh,
    repo: &str,
    n: u64,
    add: &[&str],
    remove: &[&str],
) -> Result<(), String> {
    gh.edit_labels(repo, n, add, remove)?;
    ledger(
        dirs,
        json!({ "kind": "label", "repo": repo, "issue": n, "add": add, "remove": remove, "at": crate::journal::now() }),
    )
}

/// The claim: queued → in-shift, plus a comment naming the shift. A self-lane issue keeps the
/// owner's `wg:self`: the factory never adds or removes that label (design §12.1).
pub fn claim(
    dirs: &Dirs,
    gh: &Gh,
    s: &Shift,
    repo: &str,
    n: u64,
    shift: &str,
) -> Result<(), String> {
    let remove: Vec<&str> = [s.label.as_str()]
        .into_iter()
        .filter(|l| *l != crate::selfimprove::github::SELF)
        .collect();
    relabel(dirs, gh, repo, n, &[IN_SHIFT], &remove)?;
    gh.comment(
        repo,
        n,
        &format!("workgraph shift `{shift}` picked this up."),
    )
}
