//! `wg eval capture --pr <url>` (design §12.4): builds a fixture from a merged PR of a configured
//! repo and pushes it as a `fixture/<n>-<slug>` branch to the private eval repo, from a throwaway
//! clone under `~/.workgraph-work/eval/capture/`, then opens a draft PR there for the owner to
//! promote by merging. It never pushes the eval repo's main (git.rs refuses every ref but
//! `fixture/*` for that remote), never merges, never forces, and never touches the pin in
//! `eval/manifest.toml`.

use crate::config::Config;
use crate::gh::Gh;
use crate::git::git;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::path::Path;

/// `https://github.com/<owner>/<repo>/pull/<n>` → (`owner/repo`, n).
pub fn parse_url(url: &str) -> Result<(String, u64), String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .ok_or("--pr must be https://github.com/<owner>/<repo>/pull/<n>")?;
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    match parts.as_slice() {
        [o, r, "pull", n] => {
            let repo = format!("{o}/{r}");
            crate::config::repo_slug(&repo)?;
            let n = n.parse().map_err(|_| "the PR number is not a number")?;
            Ok((repo, n))
        }
        _ => Err("--pr must be https://github.com/<owner>/<repo>/pull/<n>".into()),
    }
}

/// Test files a PR changed: a path segment `test`/`tests`/`spec`/`__tests__`, or a file named
/// `test_*`, `*_test.*`, `*.test.*` or `*.spec.*`.
pub fn is_test(path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').collect();
    let file = segs.last().copied().unwrap_or("");
    segs.iter()
        .any(|s| ["test", "tests", "spec", "__tests__"].contains(s))
        || file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
}

fn issue_of(body: &str) -> Option<u64> {
    body.lines()
        .find_map(|l| l.trim().strip_prefix("Closes #"))
        .and_then(|n| n.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

pub fn capture(
    dirs: &Dirs,
    cfg: &Config,
    gh: &Gh,
    url: &str,
    expect: &str,
) -> Result<Value, String> {
    if !["done", "needs_decision"].contains(&expect) {
        return Err("--expect is done or needs_decision".into());
    }
    let (repo, n) = parse_url(url)?;
    let rc = cfg
        .repo(&repo)
        .ok_or_else(|| format!("{repo} is not a [[repo]] in factory.toml"))?;
    let sb = rc
        .sandbox
        .as_ref()
        .ok_or_else(|| format!("{repo} has no [repo.sandbox]: a fixture needs its image"))?;
    let pr = gh.json(&[
        "pr",
        "view",
        &n.to_string(),
        "-R",
        &repo,
        "--json",
        "number,title,body,state,mergeCommit",
    ])?;
    let merge = pr["mergeCommit"]["oid"].as_str().unwrap_or("");
    if pr["state"] != "MERGED" || !super::install::sha_ok(merge) {
        return Err(format!("{repo}#{n} is not a merged PR"));
    }
    let title = pr["title"].as_str().unwrap_or("").to_string();
    let body = pr["body"].as_str().unwrap_or("").to_string();
    let task = match issue_of(&body) {
        Some(i) => {
            let issue = gh.issue(&repo, i)?;
            format!("{}\n\n{}", issue.title, issue.body)
        }
        None => format!("{title}\n\n{body}"),
    };
    let src = Path::new(&rc.path);
    crate::git::fetch(src)?;
    let base = git(src, &["rev-parse", &format!("{merge}^1")], &[])?;
    let name = format!("{n}-{}", crate::untrusted::issue::slug(&title));
    let branch = format!("fixture/{name}");
    if !crate::git::fixture_branch(&branch) {
        return Err(format!("{branch} is not a fixture branch name"));
    }
    let work = dirs.eval_work().join("capture").join(&name);
    if work.exists() {
        std::fs::remove_dir_all(&work)
            .map_err(|e| format!("cannot clear {}: {e}", work.display()))?;
    }
    crate::state_dirs::private_dir(&work)?;
    let remote = &cfg.self_improve.eval_remote;
    git(&work, &["clone", "-q", remote, "store"], &[])?;
    let store = work.join("store");
    let has_main = git(
        &store,
        &["rev-parse", "--verify", "-q", "refs/remotes/origin/main"],
        &[],
    )
    .is_ok();
    if has_main {
        git(
            &store,
            &["checkout", "-q", "-b", &branch, "origin/main"],
            &[],
        )?;
    } else {
        git(&store, &["checkout", "-q", "--orphan", &branch], &[])?;
    }
    let fx = store.join("fixtures").join(&name);
    crate::state_dirs::private_dir(&fx.join("hidden"))?;
    // The repo at the base commit, as a bundle, so the eval never needs the source repo's remote.
    let tmp_ref = format!("refs/wg-fixture/{name}");
    git(src, &["update-ref", &tmp_ref, &base], &[])?;
    let bundled = git(
        src,
        &[
            "bundle",
            "create",
            &fx.join("repo.bundle").to_string_lossy(),
            &tmp_ref,
        ],
        &[],
    );
    git(src, &["update-ref", "-d", &tmp_ref], &[])?;
    bundled?;
    let changed = git(src, &["diff", "--name-only", "-z", &base, merge], &[])?;
    let mut hidden = Vec::new();
    for p in changed.split('\0').filter(|p| !p.is_empty() && is_test(p)) {
        let Ok(text) = git(src, &["show", &format!("{merge}:{p}")], &[]) else {
            continue; // deleted by the PR
        };
        let to = fx.join("hidden").join(p);
        crate::state_dirs::private_dir(to.parent().unwrap_or(&fx))?;
        std::fs::write(&to, text + "\n").map_err(|e| e.to_string())?;
        hidden.push(p.to_string());
    }
    std::fs::write(fx.join("hidden/.keep"), "").map_err(|e| e.to_string())?;
    std::fs::write(fx.join("task.md"), &task).map_err(|e| e.to_string())?;
    let checks: Vec<String> = sb
        .checks
        .iter()
        .map(|(k, v)| format!("{k} = {}", toml_str(v)))
        .collect();
    let toml = format!(
        "# Captured from {url} by wg eval capture.\nrepo = {}\nbase_commit = {}\nexpected_status = {}\nimage = {}\nchecks = {{ {} }}\n",
        toml_str(&repo), toml_str(&base), toml_str(expect), toml_str(&sb.image), checks.join(", ")
    );
    std::fs::write(fx.join("fixture.toml"), toml).map_err(|e| e.to_string())?;
    git(&store, &["add", "-A", "fixtures"], &[])?;
    git(
        &store,
        &[
            "-c",
            "user.name=workgraph",
            "-c",
            "user.email=workgraph@localhost",
            "commit",
            "-qm",
            &format!("feat: fixture {name} from {repo}#{n}"),
        ],
        &[],
    )?;
    crate::git::push_fixture(&store, remote, &branch)?;
    let draft = gh.pr_create(
        &cfg.self_improve.eval_repo,
        &branch,
        "main",
        &format!("feat: fixture {name}"),
        &format!("Fixture captured from {url} (expected status `{expect}`). Merge to promote it; the pin in workgraph's eval/manifest.toml is yours to bump."),
        true,
        &[],
    )?;
    Ok(
        json!({ "fixture": name, "branch": branch, "base": base, "hidden": hidden, "draft_pr": draft }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_tests_and_issue_links() {
        assert_eq!(
            parse_url("https://github.com/pxm-0/kadath/pull/12").unwrap(),
            ("pxm-0/kadath".into(), 12)
        );
        for bad in [
            "http://github.com/o/r/pull/1",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/x",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
        for t in [
            "tests/a.rs",
            "src/test_calc.py",
            "a/b_test.go",
            "x.test.ts",
            "y.spec.js",
            "src/__tests__/z.js",
        ] {
            assert!(is_test(t), "{t}");
        }
        assert!(!is_test("src/calc.py") && !is_test("contest.md"));
        assert_eq!(issue_of("Closes #42\n\nmore"), Some(42));
        assert_eq!(issue_of("no link"), None);
    }
}
