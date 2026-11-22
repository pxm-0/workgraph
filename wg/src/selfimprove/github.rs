//! The GitHub reads and writes of the self lane, through the kernel's `Gh` (fixed argv, bodies by
//! file, retries). Every reply is untrusted and parsed field by field (design §12.3).

use crate::gh::Gh;
use serde_json::Value;

pub const SELF: &str = "wg:self";
pub const HELD: &str = "wg:self-held";
pub const SHADOW: &str = "wg:self-shadow";
pub const PROPOSED: &str = "wg:self-proposed";

fn sha_ok(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// The PR facts step 1 of the gate needs.
#[derive(Clone, Debug, PartialEq)]
pub struct SelfPr {
    pub number: u64,
    pub state: String,
    pub head: String,
    pub base_ref: String,
    pub labels: Vec<String>,
    pub draft: bool,
    pub mergeable: String,
    /// `(oid, full message)` of every commit.
    pub commits: Vec<(String, String)>,
}

pub fn self_pr(gh: &Gh, repo: &str, n: u64) -> Result<SelfPr, String> {
    crate::config::repo_slug(repo)?;
    let v = gh.json(&[
        "pr",
        "view",
        &n.to_string(),
        "-R",
        repo,
        "--json",
        "number,state,headRefOid,baseRefName,labels,isDraft,mergeable,commits",
    ])?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let head = s("headRefOid");
    if !sha_ok(&head) {
        return Err(format!("gh pr view {n}: no head commit"));
    }
    let commits = v["commits"]
        .as_array()
        .ok_or("gh pr view: no commit list")?
        .iter()
        .map(|c| {
            let oid = c["oid"].as_str().unwrap_or_default().to_string();
            let msg = format!(
                "{}\n\n{}",
                c["messageHeadline"].as_str().unwrap_or_default(),
                c["messageBody"].as_str().unwrap_or_default()
            );
            if sha_ok(&oid) {
                Ok((oid, msg))
            } else {
                Err("gh pr view: a commit without an oid".to_string())
            }
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(SelfPr {
        number: v["number"].as_u64().ok_or("gh pr view: no number")?,
        state: s("state"),
        head,
        base_ref: s("baseRefName"),
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(str::to_string))
            .collect(),
        draft: v["isDraft"].as_bool().unwrap_or(true),
        mergeable: s("mergeable"),
        commits,
    })
}

/// The `Workgraph-Run:` trailer of a commit message, if it has exactly one well-formed one.
pub fn run_trailer(msg: &str) -> Option<String> {
    let runs: Vec<&str> = msg
        .lines()
        .filter_map(|l| l.strip_prefix("Workgraph-Run: "))
        .map(str::trim)
        .collect();
    match runs.as_slice() {
        [r] if r.starts_with("r-")
            && r.len() <= 40
            && r[2..].chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            Some(r.to_string())
        }
        _ => None,
    }
}

pub fn open_self_prs(gh: &Gh, repo: &str) -> Result<Vec<u64>, String> {
    crate::config::repo_slug(repo)?;
    let v = gh.json(&[
        "pr", "list", "-R", repo, "--label", SELF, "--state", "open", "--json", "number",
    ])?;
    Ok(v.as_array()
        .ok_or("gh pr list: not a list")?
        .iter()
        .filter_map(|p| p["number"].as_u64())
        .collect())
}

/// One check-run on a commit, with the workflow file its suite ran (design §12.3 step 3).
#[derive(Clone, Debug, PartialEq)]
pub struct CheckRun {
    pub name: String,
    pub head_sha: String,
    pub app: String,
    pub status: String,
    pub conclusion: String,
    pub workflow: Option<String>,
}

/// Check-runs on `sha`, each joined to the workflow run of its check suite.
pub fn check_runs(gh: &Gh, repo: &str, sha: &str) -> Result<Vec<CheckRun>, String> {
    crate::config::repo_slug(repo)?;
    if !sha_ok(sha) {
        return Err(format!("{sha:?} is not a commit id"));
    }
    let runs = gh.lines(&[
        "api", "--paginate", &format!("repos/{repo}/commits/{sha}/check-runs"), "--jq",
        ".check_runs[] | {name: .name, head_sha: .head_sha, app: .app.slug, status: .status, conclusion: .conclusion, suite: .check_suite.id}",
    ])?;
    let wf = gh.lines(&[
        "api",
        "--paginate",
        &format!("repos/{repo}/actions/runs?head_sha={sha}"),
        "--jq",
        ".workflow_runs[] | {suite: .check_suite_id, path: .path, head_sha: .head_sha}",
    ])?;
    let path_of = |suite: &Value| {
        wf.iter()
            .find(|w| &w["suite"] == suite && suite.is_u64() && w["head_sha"] == sha)
            .and_then(|w| w["path"].as_str())
            // A run's `path` may carry a `@ref` suffix; only the file counts.
            .map(|p| p.split('@').next().unwrap_or(p).to_string())
    };
    Ok(runs
        .iter()
        .map(|r| {
            let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            CheckRun {
                name: s("name"),
                head_sha: s("head_sha"),
                app: s("app"),
                status: s("status"),
                conclusion: s("conclusion"),
                workflow: path_of(&r["suite"]),
            }
        })
        .collect())
}

/// Required checks on `sha` (design §12.3 step 3): `Ok(None)` when every pair passed, `Ok(Some)`
/// with what failed, and `Err` with what is still running.
pub fn required(
    runs: &[CheckRun],
    sha: &str,
    pairs: &[(String, String)],
) -> Result<Option<String>, String> {
    let mut failed = Vec::new();
    let mut pending = Vec::new();
    for (wf, job) in pairs {
        let mine: Vec<&CheckRun> = runs
            .iter()
            .filter(|r| {
                &r.name == job
                    && r.head_sha == sha
                    && r.app == "github-actions"
                    && r.workflow.as_deref() == Some(wf.as_str())
            })
            .collect();
        if mine
            .iter()
            .any(|r| r.status == "completed" && r.conclusion == "success")
        {
            continue;
        }
        // No run yet means CI has not reported: wait. A forged or misplaced run never matches.
        if !mine.is_empty() && mine.iter().all(|r| r.status == "completed") {
            failed.push(format!("{wf} {job}"));
        } else {
            pending.push(format!("{wf} {job}"));
        }
    }
    if !failed.is_empty() {
        return Ok(Some(format!(
            "required checks not passed: {}",
            failed.join(", ")
        )));
    }
    if !pending.is_empty() {
        return Err(format!(
            "required checks still running: {}",
            pending.join(", ")
        ));
    }
    Ok(None)
}

/// Adds a gate label to PR `n`, creating it first: `gh pr edit --add-label` fails on a label the
/// repo does not have.
pub fn label_pr(gh: &Gh, repo: &str, n: u64, label: &str) -> Result<(), String> {
    crate::config::repo_slug(repo)?;
    crate::ids::gh_label(label)?;
    let (color, what) = match label {
        SHADOW => (
            "0E8A16",
            "workgraph: the self gate would merge this (shadow mode)",
        ),
        _ => (
            "D93F0B",
            "workgraph: the self gate held or refused this; the owner's now",
        ),
    };
    gh.run(&[
        "label",
        "create",
        label,
        "-R",
        repo,
        "--color",
        color,
        "--description",
        what,
        "--force",
    ])?;
    gh.run(&[
        "pr",
        "edit",
        &n.to_string(),
        "-R",
        repo,
        "--add-label",
        label,
    ])
    .map(|_| ())
}

pub fn comment_pr(gh: &Gh, repo: &str, n: u64, body: &str) -> Result<(), String> {
    crate::config::repo_slug(repo)?;
    gh.with_body_file(body, |f| {
        gh.run(&[
            "pr",
            "comment",
            &n.to_string(),
            "-R",
            repo,
            "--body-file",
            f,
        ])
        .map(|_| ())
    })
}

/// Files an issue and returns its number. The label is checked by `gh_label`, so `wg:self` can
/// never be one (design §12.1).
pub fn create_issue(
    gh: &Gh,
    repo: &str,
    title: &str,
    body: &str,
    label: &str,
) -> Result<u64, String> {
    crate::config::repo_slug(repo)?;
    crate::ids::gh_label(label)?;
    let title = crate::ids::one_line(title, 120);
    let out = gh.with_body_file(body, |f| {
        gh.run(&[
            "issue",
            "create",
            "-R",
            repo,
            "--title",
            &title,
            "--body-file",
            f,
            "--label",
            label,
        ])
    })?;
    out.lines()
        .rev()
        .find_map(|l| {
            l.trim()
                .rsplit_once("/issues/")
                .and_then(|(_, n)| n.parse().ok())
        })
        .ok_or_else(|| "gh issue create printed no issue URL".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, wf: Option<&str>, status: &str, conclusion: &str) -> CheckRun {
        CheckRun {
            name: name.into(),
            head_sha: "a".repeat(40),
            app: "github-actions".into(),
            status: status.into(),
            conclusion: conclusion.into(),
            workflow: wf.map(str::to_string),
        }
    }

    #[test]
    fn required_checks_match_name_workflow_app_and_head() {
        let h = "a".repeat(40);
        let pairs = vec![
            (".github/workflows/ci.yml".to_string(), "test".to_string()),
            (
                ".github/workflows/self-gate.yml".to_string(),
                "lint-editable".to_string(),
            ),
        ];
        let ok = [
            run(
                "test",
                Some(".github/workflows/ci.yml"),
                "completed",
                "success",
            ),
            run(
                "lint-editable",
                Some(".github/workflows/self-gate.yml"),
                "completed",
                "success",
            ),
        ];
        assert_eq!(required(&ok, &h, &pairs), Ok(None));
        // A job named `test` in another workflow does not count.
        let other = [
            run(
                "test",
                Some(".github/workflows/evil.yml"),
                "completed",
                "success",
            ),
            ok[1].clone(),
        ];
        assert!(required(&other, &h, &pairs)
            .unwrap_err()
            .contains("ci.yml test"));
        let forged = [
            CheckRun {
                app: "my-app".into(),
                ..ok[0].clone()
            },
            ok[1].clone(),
        ];
        assert!(
            required(&forged, &h, &pairs).is_err(),
            "a forged run is never evidence"
        );
        let stale = [
            CheckRun {
                head_sha: "b".repeat(40),
                ..ok[0].clone()
            },
            ok[1].clone(),
        ];
        assert!(
            required(&stale, &h, &pairs).is_err(),
            "nor one on another head"
        );
        let failed = [
            run(
                "test",
                Some(".github/workflows/ci.yml"),
                "completed",
                "failure",
            ),
            ok[1].clone(),
        ];
        assert!(required(&failed, &h, &pairs).unwrap().is_some());
        let running = [
            run("test", Some(".github/workflows/ci.yml"), "in_progress", ""),
            ok[1].clone(),
        ];
        assert!(required(&running, &h, &pairs)
            .unwrap_err()
            .contains("still running"));
    }

    #[test]
    fn trailers_are_one_well_formed_run() {
        assert_eq!(
            run_trailer("feat: x\n\nWorkgraph-Run: r-abc123\n").as_deref(),
            Some("r-abc123")
        );
        assert_eq!(run_trailer("feat: x"), None);
        assert_eq!(run_trailer("Workgraph-Run: r-1\nWorkgraph-Run: r-2"), None);
        assert_eq!(run_trailer("Workgraph-Run: owner"), None);
    }
}
