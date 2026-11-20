//! The task record, `~/.workgraph/tasks/<owner>__<repo>#<n>.json` (design §7.4): the one record
//! that links every run of an issue (worktree, branch, task id, runs, PR). Saved atomically;
//! every change builds a new value.

use crate::state_dirs::Dirs;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct TaskRecord {
    pub repo: String,
    pub issue: u64,
    pub title: String,
    pub worktree: String,
    pub branch: String,
    pub task_id: Option<String>,
    pub initialized: bool,
    pub installed: bool,
    pub runs: Vec<String>,
    pub body_sha256: String,
    /// `in-shift`, `pr-open`, `needs-decision`, `stuck`, `parked`, `incident`, `approved` (finalize
    /// passed, the push or `gh pr ready` still to do), `verified`, `gone`.
    pub status: String,
    /// The local evidence commit `wg approve` made, while the record is `approved`.
    #[serde(default)]
    pub approved_head: Option<String>,
    pub pr: Option<u64>,
    pub pr_url: Option<String>,
    pub claimed_at: u64,
    pub updated_at: u64,
    pub shift: String,
}

pub fn slug(repo: &str) -> String {
    repo.replacen('/', "__", 1)
}

pub fn path(dirs: &Dirs, repo: &str, issue: u64) -> std::path::PathBuf {
    dirs.task_record(&slug(repo), issue)
}

pub fn load(dirs: &Dirs, repo: &str, issue: u64) -> Result<Option<TaskRecord>, String> {
    read(&path(dirs, repo, issue))
}

fn read(p: &Path) -> Result<Option<TaskRecord>, String> {
    match std::fs::read_to_string(p) {
        Ok(t) => serde_json::from_str(&t)
            .map(Some)
            .map_err(|e| format!("task record {} is not valid: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", p.display())),
    }
}

pub fn save(dirs: &Dirs, r: &TaskRecord) -> Result<(), String> {
    let p = path(dirs, &r.repo, r.issue);
    write_atomic(
        &p,
        &(serde_json::to_string_pretty(r).expect("serializes") + "\n"),
    )
}

/// Writes `text` to `p` through a temporary file and a rename, in a private directory.
pub fn write_atomic(p: &Path, text: &str) -> Result<(), String> {
    if let Some(d) = p.parent() {
        crate::state_dirs::private_dir(d)?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("cannot replace {}: {e}", p.display()))
}

/// Every task record, oldest claim first.
pub fn all(dirs: &Dirs) -> Result<Vec<TaskRecord>, String> {
    let dir = dirs.state().join("tasks");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for e in entries {
        let p = e.map_err(|e| e.to_string())?.path();
        if p.extension().is_some_and(|x| x == "json") {
            out.extend(read(&p)?);
        }
    }
    out.sort_by_key(|r| (r.claimed_at, r.issue));
    Ok(out)
}

/// The issues of `repo` that have a task record (each later take of one is a rerun).
pub fn issues_of(dirs: &Dirs, repo: &str) -> Result<Vec<u64>, String> {
    Ok(all(dirs)?
        .into_iter()
        .filter(|r| r.repo == repo)
        .map(|r| r.issue)
        .collect())
}

/// The record of the task whose PR is `pr` in `repo`.
pub fn by_pr(dirs: &Dirs, repo: &str, pr: u64) -> Result<TaskRecord, String> {
    all(dirs)?
        .into_iter()
        .find(|r| r.repo == repo && r.pr == Some(pr))
        .ok_or_else(|| format!("no shift task of {repo} has PR #{pr}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_are_found_by_pr() {
        let home = std::env::temp_dir().join(format!("wg-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dirs = Dirs { home };
        assert_eq!(all(&dirs).unwrap(), vec![]);
        assert_eq!(load(&dirs, "o/r", 4).unwrap(), None);
        let r = TaskRecord {
            repo: "o/r".into(),
            issue: 4,
            status: "pr-open".into(),
            pr: Some(9),
            ..Default::default()
        };
        save(&dirs, &r).unwrap();
        save(
            &dirs,
            &TaskRecord {
                issue: 5,
                pr: None,
                claimed_at: 1,
                ..r.clone()
            },
        )
        .unwrap();
        assert_eq!(load(&dirs, "o/r", 4).unwrap(), Some(r.clone()));
        assert_eq!(by_pr(&dirs, "o/r", 9).unwrap(), r);
        assert!(by_pr(&dirs, "o/r", 10)
            .unwrap_err()
            .contains("no shift task"));
        assert_eq!(all(&dirs).unwrap().len(), 2);
        std::fs::write(path(&dirs, "o/r", 6), "{").unwrap();
        assert!(load(&dirs, "o/r", 6).unwrap_err().contains("not valid"));
    }
}
