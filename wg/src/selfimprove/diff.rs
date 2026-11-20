//! Gate step 2 (design §12.3): the PR's diff computed locally with rename detection off, never
//! taken from an LLM or the PR text; every side of every entry against the strictest protected
//! list; the line and file caps; lint-editable on the head's editable Rust; and the head's
//! prompts and pipelines loaded as an overlay by this (base) kernel's pipeline loader.

use super::protected::{self, Change, Rules, Verdict};
use crate::git::git;
use std::path::{Path, PathBuf};

pub struct Diff {
    pub base: String,
    pub changes: Vec<Change>,
    pub verdict: Verdict,
}

/// Fetches, then makes sure `head` is present (a PR ref on GitHub, a branch locally).
pub fn fetch(repo: &Path, pr: u64, head: &str) -> Result<(), String> {
    crate::git::fetch(repo)?;
    let have = |h: &str| git(repo, &["cat-file", "-e", &format!("{h}^{{commit}}")], &[]).is_ok();
    if !have(head) {
        // Best effort: a fork-less PR's head is a branch the plain fetch already brought.
        let _ = git(
            repo,
            &["fetch", "-q", "origin", &format!("refs/pull/{pr}/head")],
            &[],
        );
    }
    if have(head) {
        Ok(())
    } else {
        Err(format!("the PR head {head} is not in origin"))
    }
}

/// The strictest of the compiled list and origin/main's (design §12.2): a PR never relaxes it.
pub fn rules(repo: &Path) -> Result<Rules, String> {
    let text = git(repo, &["show", "origin/main:config/protected.toml"], &[])
        .map_err(|e| format!("origin/main has no readable config/protected.toml: {e}"))?;
    Ok(protected::compiled().strictest(&protected::parse(&text)?))
}

pub fn diff(repo: &Path, rules: &Rules, head: &str) -> Result<Diff, String> {
    let base = git(repo, &["rev-parse", "origin/main"], &[])?;
    let range = format!("{base}...{head}");
    let z = git(
        repo,
        &["diff", "--no-renames", "--name-status", "-z", &range],
        &[],
    )?;
    let changes = protected::parse_name_status(&z)?;
    let numstat = git(
        repo,
        &["diff", "--no-renames", "--numstat", "-z", &range],
        &[],
    )?;
    let verdict = protected::verdict(rules, &changes, protected::changed_lines(&numstat));
    Ok(Diff {
        base,
        changes,
        verdict,
    })
}

/// lint-editable over the head's version of every changed editable Rust file.
pub fn lint(repo: &Path, head: &str, changes: &[Change]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for c in changes
        .iter()
        .filter(|c| c.status != 'D' && super::lint::covers(&c.path))
    {
        let src = git(repo, &["show", &format!("{head}:{}", c.path)], &[])?;
        out.extend(
            super::lint::findings(&src)
                .into_iter()
                .map(|f| format!("{} {f}", c.path)),
        );
    }
    Ok(out)
}

/// Prompts and pipelines the PR changed, as `(dir, file name)`.
pub fn asset_changes(changes: &[Change]) -> Vec<(String, String)> {
    changes
        .iter()
        .filter(|c| c.status != 'D')
        .filter_map(|c| {
            let (dir, file) = c.path.split_once('/')?;
            let flat = !file.contains('/');
            let ok = (dir == "prompts" && file.ends_with(".md"))
                || (dir == "pipelines" && file.ends_with(".toml"));
            (flat && ok).then(|| (dir.to_string(), file.to_string()))
        })
        .collect()
}

/// Writes the head's changed prompts and pipelines under `dir` (a fresh overlay).
pub fn write_overlay(
    repo: &Path,
    head: &str,
    files: &[(String, String)],
    dir: &Path,
) -> Result<PathBuf, String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("cannot clear {}: {e}", dir.display()))?;
    }
    for (sub, f) in files {
        crate::state_dirs::private_dir(&dir.join(sub))?;
        let text = git(repo, &["show", &format!("{head}:{sub}/{f}")], &[])?;
        std::fs::write(dir.join(sub).join(f), text + "\n")
            .map_err(|e| format!("cannot write the overlay: {e}"))?;
    }
    Ok(dir.to_path_buf())
}

/// Loads every chain the overlay touches with this kernel's loader: dev+qa, and the release line.
pub fn overlay_loads(overlay: &Path, max_calls: u32) -> Result<(), String> {
    let a = crate::assets::Assets::compiled().overlay(overlay)?;
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let view = crate::pipelines::RepoView::default();
    crate::pipelines::chain(&a, &names(&["dev", "qa"]), &view, max_calls)?;
    let release = crate::pipelines::RepoView {
        rehearsal: true,
        ..view
    };
    crate::pipelines::chain(&a, &names(&["release"]), &release, max_calls)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_flat_prompt_and_pipeline_files_form_the_overlay() {
        let c = |s: char, p: &str| Change {
            status: s,
            path: p.into(),
        };
        let got = asset_changes(&[
            c('M', "prompts/work.md"),
            c('A', "prompts/deep/x.md"),
            c('M', "pipelines/dev.toml"),
            c('D', "prompts/old.md"),
            c('M', "docs/a.md"),
        ]);
        assert_eq!(
            got,
            [
                ("prompts".to_string(), "work.md".to_string()),
                ("pipelines".to_string(), "dev.toml".to_string())
            ]
        );
    }

    #[test]
    fn the_base_loader_refuses_a_candidate_pipeline_that_drops_haruspex() {
        let dir = std::env::temp_dir().join(format!("wg-overlay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pipelines")).unwrap();
        assert!(overlay_loads(&dir, 60).is_ok(), "the shipped assets load");
        std::fs::write(
            dir.join("pipelines/dev.toml"),
            crate::assets::DEV_PIPELINE.replace("haruspex = true", "haruspex = false"),
        )
        .unwrap();
        assert!(overlay_loads(&dir, 60)
            .unwrap_err()
            .contains("haruspex must be true"));
    }
}
