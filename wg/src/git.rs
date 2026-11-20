//! Git queries. Nothing here writes the owner's index or refs: the tree fingerprint uses a
//! temporary index file.

use sha2::{Digest, Sha256};
use std::path::Path;
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
    use super::{parse_porcelain_z, slug_of_url};

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
