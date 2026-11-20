//! The `gh` CLI, with fixed argv only (design §3.1, §7.3). Every reply is untrusted: callers parse
//! it through `untrusted::issue`. Bodies go through `--body-file`, never argv. The factory never
//! adds the label `wg:self` (design §12.1), and a failing call is retried twice (2 s, 8 s) before
//! the error reaches the caller (design §13.1).

use crate::untrusted::issue::{self, Edit, Issue, LabelEvent};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

pub const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];

/// Longest reply read from `gh`; anything larger is refused rather than parsed.
const MAX_REPLY: usize = 8 * 1024 * 1024;

const EDITS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){issue(number:$number){userContentEdits(first:100){nodes{editedAt editor{login}}}}}}";

#[derive(Clone, Debug)]
pub struct Gh {
    /// Absolute path from `factory.toml [tools].gh`.
    pub bin: String,
    pub delays: Vec<Duration>,
    /// Where body files are written: a private dir under `~/.workgraph`.
    pub scratch: PathBuf,
}

/// An open or closed PR, as `gh pr list` / `gh pr view` report it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    pub state: String,
    pub head_oid: String,
}

fn pr_of(v: &Value) -> Result<Pr, String> {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let number = v["number"].as_u64().ok_or("gh: a PR without a number")?;
    let url = s("url");
    if !url.starts_with("https://") && !url.starts_with("http://") || url.len() > 300 {
        return Err("gh: a PR url is not a URL".into());
    }
    Ok(Pr {
        number,
        url,
        state: s("state"),
        head_oid: s("headRefOid"),
    })
}

fn check_repo(repo: &str) -> Result<(), String> {
    crate::config::repo_slug(repo)
}

impl Gh {
    pub fn new(bin: &str, scratch: PathBuf) -> Gh {
        Gh {
            bin: bin.to_string(),
            delays: RETRY_DELAYS.to_vec(),
            scratch,
        }
    }

    /// Runs `gh args`, retrying a failure after each delay; stdout on success.
    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut last = String::new();
        for attempt in 0..=self.delays.len() {
            if attempt > 0 {
                std::thread::sleep(self.delays[attempt - 1]);
            }
            match Command::new(&self.bin)
                .args(args)
                .env_remove("WG_AGENT")
                .output()
            {
                Ok(o) if o.status.success() && o.stdout.len() <= MAX_REPLY => {
                    return Ok(String::from_utf8_lossy(&o.stdout).into_owned())
                }
                Ok(o) if o.status.success() => return Err("gh: reply too large".into()),
                Ok(o) => {
                    last = format!(
                        "gh {} failed ({}): {}",
                        args.first().unwrap_or(&""),
                        o.status.code().unwrap_or(-1),
                        crate::exec::run::tail(String::from_utf8_lossy(&o.stderr).trim())
                    )
                }
                Err(e) => last = format!("gh failed to start: {e}"),
            }
        }
        Err(last)
    }

    fn json(&self, args: &[&str]) -> Result<Value, String> {
        let text = self.run(args)?;
        serde_json::from_str(&text).map_err(|e| format!("gh {}: not JSON: {e}", args[0]))
    }

    /// One JSON object per line (`--jq` output).
    fn lines(&self, args: &[&str]) -> Result<Vec<Value>, String> {
        self.run(args)?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| format!("gh {}: not JSON: {e}", args[0])))
            .collect()
    }

    pub fn auth_status(&self) -> Result<(), String> {
        self.run(&["auth", "status"]).map(|_| ())
    }

    /// Open issues carrying `label`, parsed and size-checked; the second list says why others
    /// were dropped.
    pub fn issues(&self, repo: &str, label: &str) -> Result<(Vec<Issue>, Vec<String>), String> {
        check_repo(repo)?;
        let v = self.json(&[
            "issue",
            "list",
            "-R",
            repo,
            "--label",
            label,
            "--state",
            "open",
            "--json",
            "number,title,body,author,labels,url,updatedAt,createdAt,state",
            "--limit",
            "20",
        ])?;
        let items = v.as_array().ok_or("gh issue list: not a list")?;
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        for item in items {
            match issue::parse(item) {
                Ok(i) => ok.push(i),
                Err(e) => bad.push(e),
            }
        }
        Ok((ok, bad))
    }

    pub fn issue(&self, repo: &str, n: u64) -> Result<Issue, String> {
        check_repo(repo)?;
        let v = self.json(&[
            "issue",
            "view",
            &n.to_string(),
            "-R",
            repo,
            "--json",
            "number,title,body,author,labels,url,updatedAt,createdAt,state",
        ])?;
        issue::parse(&v)
    }

    /// Issue `n`'s `labeled` events and its title renames (as edits), oldest first.
    pub fn timeline(&self, repo: &str, n: u64) -> Result<(Vec<LabelEvent>, Vec<Edit>), String> {
        check_repo(repo)?;
        let path = format!("repos/{repo}/issues/{n}/timeline");
        let rows = self.lines(&[
            "api", "--paginate", &path, "--jq",
            r#".[] | select(.event == "labeled" or .event == "renamed") | {event: .event, label: .label.name, actor: .actor.login, at: .created_at}"#,
        ])?;
        let mut labels = Vec::new();
        let mut renames = Vec::new();
        for r in &rows {
            if r["event"] == "renamed" {
                renames.push(issue::rename(r)?);
            } else {
                labels.push(issue::label_event(r)?);
            }
        }
        Ok((labels, renames))
    }

    /// Every edit of issue `n`'s body.
    pub fn body_edits(&self, repo: &str, n: u64) -> Result<Vec<Edit>, String> {
        check_repo(repo)?;
        let (owner, name) = repo.split_once('/').ok_or("repo must be owner/name")?;
        let rows = self.lines(&[
            "api", "graphql", "-f", &format!("query={EDITS_QUERY}"),
            "-F", &format!("owner={owner}"), "-F", &format!("name={name}"),
            "-F", &format!("number={n}"), "--jq",
            ".data.repository.issue.userContentEdits.nodes[] | {at: .editedAt, editor: .editor.login}",
        ])?;
        rows.iter().map(issue::edit).collect()
    }

    /// Absolute label edits: idempotent, so a resume can repeat them.
    pub fn edit_labels(
        &self,
        repo: &str,
        n: u64,
        add: &[&str],
        remove: &[&str],
    ) -> Result<(), String> {
        check_repo(repo)?;
        let num = n.to_string();
        let mut args = vec!["issue", "edit", num.as_str(), "-R", repo];
        for l in add {
            crate::ids::gh_label(l)?;
            args.extend(["--add-label", l]);
        }
        for l in remove {
            crate::ids::gh_label(l)?;
            args.extend(["--remove-label", l]);
        }
        self.run(&args).map(|_| ())
    }

    fn body_file(&self, body: &str) -> Result<PathBuf, String> {
        crate::state_dirs::private_dir(&self.scratch)?;
        let p = self
            .scratch
            .join(format!("body-{}.md", crate::journal::now_nanos()));
        std::fs::write(&p, body).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
        Ok(p)
    }

    pub fn comment(&self, repo: &str, n: u64, body: &str) -> Result<(), String> {
        check_repo(repo)?;
        let f = self.body_file(body)?;
        let f = f.to_string_lossy();
        self.run(&[
            "issue",
            "comment",
            &n.to_string(),
            "-R",
            repo,
            "--body-file",
            &f,
        ])
        .map(|_| ())
    }

    /// The PR whose head is `branch`, in any state.
    pub fn pr_for_head(&self, repo: &str, branch: &str) -> Result<Option<Pr>, String> {
        check_repo(repo)?;
        let v = self.json(&[
            "pr",
            "list",
            "-R",
            repo,
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,url,state,headRefOid",
        ])?;
        v.as_array()
            .ok_or("gh pr list: not a list")?
            .first()
            .map(pr_of)
            .transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pr_create(
        &self,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
        labels: &[String],
    ) -> Result<String, String> {
        check_repo(repo)?;
        let f = self.body_file(body)?;
        let f = f.to_string_lossy();
        let mut args = vec![
            "pr",
            "create",
            "-R",
            repo,
            "--head",
            head,
            "--base",
            base,
            "--title",
            title,
            "--body-file",
            &f,
        ];
        if draft {
            args.push("--draft");
        }
        for l in labels {
            crate::ids::gh_label(l)?;
            args.extend(["--label", l.as_str()]);
        }
        let out = self.run(&args)?;
        let url = out
            .lines()
            .rev()
            .find(|l| l.starts_with("http"))
            .unwrap_or("")
            .trim();
        if url.is_empty() {
            return Err("gh pr create printed no URL".into());
        }
        Ok(url.to_string())
    }

    /// Creates or updates a `risky:<category>` label, so the PR can carry it (design §8.1).
    pub fn ensure_label(&self, repo: &str, name: &str) -> Result<(), String> {
        check_repo(repo)?;
        crate::ids::gh_label(name)?;
        self.run(&[
            "label",
            "create",
            name,
            "-R",
            repo,
            "--color",
            "B60205",
            "--description",
            "workgraph: built and checked only in the sandbox",
            "--force",
        ])
        .map(|_| ())
    }

    pub fn pr_view(&self, repo: &str, n: u64) -> Result<Pr, String> {
        check_repo(repo)?;
        let v = self.json(&[
            "pr",
            "view",
            &n.to_string(),
            "-R",
            repo,
            "--json",
            "number,url,state,headRefOid",
        ])?;
        pr_of(&v)
    }

    pub fn pr_ready(&self, repo: &str, n: u64) -> Result<(), String> {
        check_repo(repo)?;
        self.run(&["pr", "ready", &n.to_string(), "-R", repo])
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(name: &str, script: &str) -> Gh {
        let dir = std::env::temp_dir().join(format!("wg-gh-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("gh");
        std::fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Gh {
            bin: bin.to_string_lossy().into_owned(),
            delays: vec![Duration::ZERO, Duration::ZERO],
            scratch: dir.join("scratch"),
        }
    }

    #[test]
    fn failures_are_retried_twice_then_reported() {
        let dir = std::env::temp_dir().join(format!("wg-gh-count-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let g = fake(
            "fail",
            &format!("echo x >> {}; echo nope >&2; exit 1", dir.display()),
        );
        let err = g.auth_status().unwrap_err();
        assert!(err.contains("nope") && err.contains("(1)"), "{err}");
        assert_eq!(std::fs::read_to_string(&dir).unwrap().lines().count(), 3);
        let missing = Gh {
            bin: "/nonexistent/gh".into(),
            ..g
        };
        assert!(missing
            .auth_status()
            .unwrap_err()
            .contains("failed to start"));
    }

    #[test]
    fn labels_are_checked_before_any_call_and_bodies_go_through_a_file() {
        let g = fake("ok", "for a; do last=$a; done; if [ \"$1\" = issue ] && [ \"$2\" = comment ]; then cat \"$last\"; fi");
        assert!(g
            .edit_labels("o/r", 1, &["wg:self"], &[])
            .unwrap_err()
            .contains("owner"));
        assert!(g
            .edit_labels("o/r", 1, &["wg:queued"], &["wg:in-shift"])
            .is_ok());
        assert!(g.edit_labels("not a repo", 1, &[], &[]).is_err());
        assert!(g.comment("o/r", 3, "hello `$(rm -rf ~)`").is_ok());
        let bodies: Vec<_> = std::fs::read_dir(&g.scratch).unwrap().collect();
        assert_eq!(bodies.len(), 1);
        assert!(g
            .pr_create("o/r", "wg/1-x", "main", "feat: x", "b", true, &[])
            .unwrap_err()
            .contains("no URL"));
    }

    #[test]
    fn pr_replies_are_parsed_and_checked() {
        let g = fake(
            "pr",
            r#"echo '[{"number":7,"url":"https://github.com/o/r/pull/7","state":"OPEN","headRefOid":"abc"}]'"#,
        );
        let pr = g.pr_for_head("o/r", "wg/1-x").unwrap().unwrap();
        assert_eq!(
            (pr.number, pr.state.as_str(), pr.head_oid.as_str()),
            (7, "OPEN", "abc")
        );
        let empty = fake("none", "echo '[]'");
        assert_eq!(empty.pr_for_head("o/r", "wg/1-x").unwrap(), None);
        assert!(pr_of(&serde_json::json!({ "number": 1, "url": "javascript:x" })).is_err());
        assert!(pr_of(&serde_json::json!({ "url": "https://x" })).is_err());
        let notjson = fake("bad", "echo nope");
        assert!(notjson.pr_view("o/r", 1).unwrap_err().contains("not JSON"));
    }
}
