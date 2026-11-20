//! `wg self install --sha S` (design §12.5): installs only a main SHA whose required check-runs
//! and release build succeeded, and, for a factory merge, whose self-ledger entry says passed.
//! The binary comes from the CI artifact (`gh run download`), never from a build on the Mac; its
//! sha256 and head SHA are verified; it unpacks to `bin/wg-<sha12>/wg`; `memory/` is snapshotted
//! for the binary being replaced; `current` flips by an atomic rename, keeping `previous`.

use super::github;
use super::ledger::Ledger;
use crate::gh::Gh;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";

/// The release matrix target this binary runs on.
pub fn target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        _ => "x86_64-unknown-linux-gnu",
    }
}

pub fn sha_ok(s: &str) -> bool {
    s.len() == 40
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// The `wg-<sha12>` directory a symlink in `bin/` points at.
pub fn link(dirs: &Dirs, name: &str) -> Option<String> {
    std::fs::read_link(dirs.bin().join(name))
        .ok()
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
        .and_then(|p| p.rsplit('/').next().map(str::to_string))
        .filter(|n| n.starts_with("wg-"))
}

/// Points `bin/<name>` at `bin/<dir>` through a temporary symlink and a rename: never half done.
pub fn point(dirs: &Dirs, name: &str, dir: &str) -> Result<(), String> {
    let tmp = dirs.bin().join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(dir, &tmp).map_err(|e| format!("cannot link {name}: {e}"))?;
    std::fs::rename(&tmp, dirs.bin().join(name)).map_err(|e| format!("cannot flip {name}: {e}"))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    crate::state_dirs::private_dir(to)?;
    for e in std::fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).map_err(|e| format!("cannot copy {}: {e}", src.display()))?;
        }
    }
    Ok(())
}

/// Copies `memory/` to `memory-snapshots/<sha12>/` (design §10.1); a missing memory dir snapshots
/// as empty.
pub fn snapshot_memory(dirs: &Dirs, sha12: &str) -> Result<(), String> {
    let to = dirs.memory_snapshot(sha12);
    if to.exists() {
        std::fs::remove_dir_all(&to)
            .map_err(|e| format!("cannot replace {}: {e}", to.display()))?;
    }
    if dirs.memory_root().exists() {
        copy_dir(&dirs.memory_root(), &to)
    } else {
        crate::state_dirs::private_dir(&to)
    }
}

/// The successful push run of the release workflow for `sha`.
fn release_run(gh: &Gh, repo: &str, sha: &str) -> Result<u64, String> {
    let v = gh.json(&[
        "run",
        "list",
        "-R",
        repo,
        "--workflow",
        "release.yml",
        "--commit",
        sha,
        "--json",
        "databaseId,headSha,conclusion,event,status",
        "--limit",
        "10",
    ])?;
    v.as_array()
        .ok_or("gh run list: not a list")?
        .iter()
        .find(|r| r["headSha"] == sha && r["conclusion"] == "success" && r["event"] == "push")
        .and_then(|r| r["databaseId"].as_u64())
        .ok_or_else(|| format!("no successful release build for {sha} on main"))
}

/// Downloads and verifies the artifact; returns the path of the verified binary.
fn fetch_binary(gh: &Gh, repo: &str, sha: &str, into: &Path) -> Result<PathBuf, String> {
    let run = release_run(gh, repo, sha)?;
    if into.exists() {
        std::fs::remove_dir_all(into).map_err(|e| e.to_string())?;
    }
    crate::state_dirs::private_dir(into)?;
    let name = format!("wg-{}", target());
    gh.run(&[
        "run",
        "download",
        &run.to_string(),
        "-R",
        repo,
        "-n",
        &name,
        "-D",
        &into.to_string_lossy(),
    ])?;
    let read = |f: &str| {
        std::fs::read_to_string(into.join(f)).map_err(|e| format!("the artifact has no {f}: {e}"))
    };
    let head = read("head_sha")?;
    if head.trim() != sha {
        return Err(format!(
            "the artifact was built from {}, not {sha}",
            head.trim()
        ));
    }
    let want = read("wg.sha256")?;
    let want = want.split_whitespace().next().unwrap_or("");
    let got = crate::haruspex::cli::sha256_file(&into.join("wg"))?;
    if want != got {
        return Err("the artifact's sha256 does not match its binary".into());
    }
    Ok(into.join("wg"))
}

/// Required CI on a main commit: every `ci.yml` pair of the protected list (self-gate runs on PRs
/// only, so it never reports on a main commit).
fn ci_passed(gh: &Gh, repo: &str, sha: &str) -> Result<(), String> {
    let rules = super::protected::compiled();
    let pairs: Vec<(String, String)> = rules
        .required_checks
        .into_iter()
        .filter(|(wf, _)| wf == ".github/workflows/ci.yml")
        .collect();
    let runs = github::check_runs(gh, repo, sha)?;
    match github::required(&runs, sha, &pairs) {
        Ok(None) => Ok(()),
        Ok(Some(f)) => Err(f),
        Err(running) => Err(running),
    }
}

pub fn install(dirs: &Dirs, gh: &Gh, repo: &str, sha: &str) -> Result<Value, String> {
    if !sha_ok(sha) {
        return Err("--sha must be a full lowercase commit id".into());
    }
    let st = super::state::load(&dirs.self_state())?;
    if st.denylist.iter().any(|d| d == sha) {
        return Err(format!("{sha} was rolled back and is denylisted"));
    }
    let ledger = Ledger::at(&dirs.self_ledger());
    let factory_merge = ledger.merges()?.iter().any(|m| m["merge"] == sha);
    if factory_merge {
        let passed = ledger
            .merges()?
            .iter()
            .any(|m| m["merge"] == sha && m["decision"].as_u64().is_some());
        if !passed {
            return Err(format!(
                "{sha} is a factory merge without a passing gate decision"
            ));
        }
    }
    ci_passed(gh, repo, sha)?;
    let sha12 = &sha[..12];
    let dir = format!("wg-{sha12}");
    let staged = fetch_binary(
        gh,
        repo,
        sha,
        &dirs.eval_work().join(format!("install-{sha12}")),
    )?;
    let replaced = link(dirs, "current");
    if let Some(cur) = &replaced {
        if cur == &dir {
            return Ok(json!({ "installed": sha, "already": true }));
        }
        snapshot_memory(dirs, cur.trim_start_matches("wg-"))?;
    }
    let dest = dirs.bin().join(&dir);
    crate::state_dirs::private_dir(&dest)?;
    std::fs::copy(&staged, dest.join("wg")).map_err(|e| format!("cannot install: {e}"))?;
    std::fs::set_permissions(dest.join("wg"), std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    if let Some(cur) = &replaced {
        point(dirs, "previous", cur)?;
    }
    point(dirs, "current", &dir)?;
    ledger.append(
        "installed",
        json!({ "sha": sha, "dir": dir, "previous": replaced, "factory_merge": factory_merge }),
    )?;
    Ok(json!({ "installed": sha, "current": dir, "previous": replaced }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlinks_flip_atomically_and_memory_snapshots() {
        let home = std::env::temp_dir().join(format!("wg-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dirs = Dirs { home };
        std::fs::create_dir_all(dirs.bin().join("wg-aaaaaaaaaaaa")).unwrap();
        assert_eq!(link(&dirs, "current"), None);
        point(&dirs, "current", "wg-aaaaaaaaaaaa").unwrap();
        point(&dirs, "current", "wg-bbbbbbbbbbbb").unwrap();
        assert_eq!(link(&dirs, "current").as_deref(), Some("wg-bbbbbbbbbbbb"));
        std::fs::create_dir_all(dirs.memory_root()).unwrap();
        std::fs::write(dirs.memory_root().join("o__r.json"), "{}").unwrap();
        snapshot_memory(&dirs, "aaaaaaaaaaaa").unwrap();
        assert!(dirs
            .memory_snapshot("aaaaaaaaaaaa")
            .join("o__r.json")
            .exists());
        assert!(sha_ok(&"a".repeat(40)) && !sha_ok(&"A".repeat(40)) && !sha_ok("abc"));
        assert!([
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu"
        ]
        .contains(&target()));
    }
}
