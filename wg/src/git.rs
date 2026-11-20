//! Git. Nothing here writes the owner's index or checkout: the tree fingerprint uses a temporary
//! index file, task work happens in worktrees under `~/.workgraph-work/worktrees`, and only `wg/*`
//! branches are ever pushed (design §14 checklist 2).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs git in `dir` and returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(env.iter().copied())
        .output()
        .map_err(|e| format!("git {} failed to start: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The `git write-tree` hash of the worktree, tracked plus untracked, ignoring `.haruspex/` and
/// ignored files, built in a temporary index so the real one is never touched (design §4.1).
pub fn tree_fingerprint(dir: &Path) -> Result<String, String> {
    let index = std::env::temp_dir().join(format!(
        "wg-index-{}-{}",
        std::process::id(),
        crate::journal::now_nanos()
    ));
    let idx = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", idx.as_str())];
    let result = (|| {
        if git(dir, &["rev-parse", "--verify", "-q", "HEAD"], &[]).is_ok() {
            git(dir, &["read-tree", "HEAD"], &env)?;
        }
        git(dir, &["add", "-A", "--", "."], &env)?;
        git(
            dir,
            &[
                "rm",
                "-r",
                "-q",
                "--cached",
                "--ignore-unmatch",
                "--",
                ".haruspex",
            ],
            &env,
        )?;
        git(dir, &["write-tree"], &env)
    })();
    // Best effort: a leftover temp index holds no state anyone reads.
    let _ = std::fs::remove_file(&index);
    result
}

pub fn branch(dir: &Path) -> Result<String, String> {
    git(dir, &["branch", "--show-current"], &[])
}

pub fn head(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "HEAD"], &[])
}

pub fn toplevel(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "--show-toplevel"], &[])
}

/// `<owner>__<repo>` from the origin URL, else `local__<sha256(path)[..12]>` (design §10.1).
pub fn slug(dir: &Path) -> String {
    let from_url = git(dir, &["remote", "get-url", "origin"], &[])
        .ok()
        .and_then(|url| slug_of_url(&url));
    from_url.unwrap_or_else(|| {
        let hash = format!("{:x}", Sha256::digest(dir.to_string_lossy().as_bytes()));
        format!("local__{}", &hash[..12])
    })
}

fn slug_of_url(url: &str) -> Option<String> {
    let path = url.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.rsplit(['/', ':']);
    let repo = parts.next()?;
    let owner = parts.next()?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    };
    (ok(owner) && ok(repo) && owner != "." && repo != ".").then(|| format!("{owner}__{repo}"))
}

/// The repo slug as `owner/repo`, the form `factory.toml` checks use.
pub fn repo_name(slug: &str) -> String {
    slug.replacen("__", "/", 1)
}

/// `f`, retried after 2 s and 8 s on failure (design §13.1).
fn retried<T>(mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut last = f();
    for d in crate::gh::RETRY_DELAYS {
        if last.is_ok() {
            break;
        }
        std::thread::sleep(d);
        last = f();
    }
    last
}

/// A task branch: `wg/<issue>-<slug>`.
pub fn task_branch(s: &str) -> bool {
    s.strip_prefix("wg/")
        .and_then(|r| r.split_once('-'))
        .is_some_and(|(n, slug)| {
            (1..=9).contains(&n.len())
                && n.chars().all(|c| c.is_ascii_digit())
                && (1..=32).contains(&slug.len())
                && slug
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// Updates the owner's remote-tracking refs only; the checkout is never touched.
pub fn fetch(repo: &Path) -> Result<(), String> {
    retried(|| git(repo, &["fetch", "-q", "origin"], &[])).map(|_| ())
}

/// True when `rev:path` exists.
pub fn exists_at(repo: &Path, rev: &str, path: &str) -> bool {
    git(repo, &["cat-file", "-e", &format!("{rev}:{path}")], &[]).is_ok()
}

/// The task worktree `<root>/<rel>` on `branch`: reused when it exists, else added from
/// `origin/<base>` (or from the kept branch on a rerun).
pub fn worktree(
    repo: &Path,
    root: &Path,
    rel: &str,
    branch: &str,
    base: &str,
) -> Result<PathBuf, String> {
    if !task_branch(branch) {
        return Err(format!("{branch:?} is not a wg/<n>-<slug> branch"));
    }
    let path = root.join(rel);
    if path.exists() {
        let on = self::branch(&path)?;
        if on != branch {
            return Err(format!("{} is on {on}, not {branch}", path.display()));
        }
        return path.canonicalize().map_err(|e| e.to_string());
    }
    crate::state_dirs::private_dir(root)?;
    let p = path.to_string_lossy();
    let kept = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
        &[],
    )
    .is_ok();
    if kept {
        git(repo, &["worktree", "add", "-q", &p, branch], &[])?;
    } else {
        git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                &p,
                &format!("origin/{base}"),
            ],
            &[],
        )?;
    }
    path.canonicalize().map_err(|e| e.to_string())
}

/// Removes a task worktree, only under `root` after canonicalizing (design §14 checklist 2).
pub fn remove_worktree(repo: &Path, root: &Path, path: &Path) -> Result<(), String> {
    let (root, p) = (
        root.canonicalize()
            .map_err(|e| format!("{}: {e}", root.display()))?,
        path.canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))?,
    );
    if !p.starts_with(&root) || p == root {
        return Err(format!(
            "refused: {} is not under {}",
            p.display(),
            root.display()
        ));
    }
    git(
        repo,
        &["worktree", "remove", "--force", &p.to_string_lossy()],
        &[],
    )
    .map(|_| ())
}

/// Stages `files` and commits them with `message`; false when there was nothing to commit.
pub fn commit(dir: &Path, files: &[String], message: &str) -> Result<bool, String> {
    if files.is_empty() {
        return Ok(false);
    }
    let mut add = vec!["add", "-A", "--"];
    add.extend(files.iter().map(String::as_str));
    git(dir, &add, &[])?;
    if git(dir, &["diff", "--cached", "--quiet"], &[]).is_ok() {
        return Ok(false);
    }
    git(dir, &["commit", "-q", "-m", message], &[])?;
    Ok(true)
}

/// Pushes a `wg/*` task branch to origin; any other ref is refused before git runs.
pub fn push_task_branch(dir: &Path, branch: &str) -> Result<(), String> {
    if !task_branch(branch) {
        return Err(format!(
            "refused to push {branch:?}: only wg/<n>-<slug> branches are pushed"
        ));
    }
    let spec = format!("refs/heads/{branch}:refs/heads/{branch}");
    retried(|| git(dir, &["push", "-q", "-u", "origin", &spec], &[])).map(|_| ())
}

/// Paths from `git status --porcelain=v1 -z`; for renames and copies, the new path.
pub fn dirty() -> Result<Vec<String>, String> {
    dirty_in(Path::new("."))
}

/// `dirty` for the repo at `dir`.
pub fn dirty_in(dir: &Path) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git status failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_porcelain_z(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_porcelain_z(text: &str) -> Vec<String> {
    let mut parts = text.split('\0').filter(|p| !p.is_empty());
    let mut paths = Vec::new();
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        if entry.starts_with('R') || entry.starts_with('C') {
            parts.next(); // skip the original path
        }
        paths.push(entry[3..].to_string());
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(ok.success(), "{script}");
    }

    #[test]
    fn worktrees_commits_and_pushes_stay_inside_the_rules() {
        let base = std::env::temp_dir().join(format!("wg-git-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("origin.git")).unwrap();
        std::fs::create_dir_all(base.join("repo")).unwrap();
        sh(&base.join("origin.git"), "git init -q --bare -b main");
        let repo = base.join("repo");
        sh(
            &repo,
            &format!(
                "git init -q -b main && git config user.name t && git config user.email t@x.invalid \
                 && echo a > a.txt && git add -A && git commit -qm init \
                 && git remote add origin {} && git push -q origin main",
                base.join("origin.git").display()
            ),
        );
        fetch(&repo).unwrap();
        assert!(
            exists_at(&repo, "origin/main", "a.txt") && !exists_at(&repo, "origin/main", "b.txt")
        );
        let root = base.join("work");
        assert!(worktree(&repo, &root, "o__r/1-x", "main", "main")
            .unwrap_err()
            .contains("not a wg/"));
        let wt = worktree(&repo, &root, "o__r/1-x", "wg/1-x", "main").unwrap();
        assert_eq!(branch(&wt).unwrap(), "wg/1-x");
        assert_eq!(
            worktree(&repo, &root, "o__r/1-x", "wg/1-x", "main").unwrap(),
            wt,
            "reused"
        );
        assert!(worktree(&repo, &root, "o__r/1-x", "wg/2-y", "main")
            .unwrap_err()
            .contains("is on wg/1-x"));
        std::fs::write(wt.join("b.txt"), "b").unwrap();
        assert!(commit(&wt, &["b.txt".into()], "feat: b").unwrap());
        assert!(!commit(&wt, &["b.txt".into()], "again").unwrap());
        assert!(!commit(&wt, &[], "nothing").unwrap());
        assert!(push_task_branch(&wt, "main")
            .unwrap_err()
            .contains("refused"));
        push_task_branch(&wt, "wg/1-x").unwrap();
        sh(
            &base.join("origin.git"),
            "git rev-parse --verify -q refs/heads/wg/1-x",
        );
        assert!(remove_worktree(&repo, &root, &repo)
            .unwrap_err()
            .contains("refused"));
        remove_worktree(&repo, &root, &wt).unwrap();
        assert!(!wt.exists());
        let again = worktree(&repo, &root, "o__r/1-x", "wg/1-x", "main").unwrap();
        assert!(
            again.join("b.txt").exists(),
            "a rerun reuses the kept branch"
        );
    }

    #[test]
    fn task_branches() {
        assert!(task_branch("wg/12-fix-login"));
        for bad in [
            "wg/x-y",
            "wg/12",
            "wg/12-",
            "work/1-x",
            "wg/1-A",
            "wg/1-a/../b",
        ] {
            assert!(!task_branch(bad), "{bad}");
        }
    }

    #[test]
    fn slugs_from_origin_urls() {
        assert_eq!(
            slug_of_url("git@github.com:pxm-0/workgraph.git").as_deref(),
            Some("pxm-0__workgraph")
        );
        assert_eq!(
            slug_of_url("https://github.com/pxm-0/kadath/").as_deref(),
            Some("pxm-0__kadath")
        );
        assert_eq!(slug_of_url("/srv/git/a b"), None);
    }

    #[test]
    fn renames_and_copies_report_the_new_path_and_short_entries_are_skipped() {
        let text = "?? b.txt\0R  new.txt\0old.txt\0C  copy.txt\0src.txt\0 M\0 M a.txt\0";
        assert_eq!(
            parse_porcelain_z(text),
            ["b.txt", "new.txt", "copy.txt", "a.txt"]
        );
    }
}
