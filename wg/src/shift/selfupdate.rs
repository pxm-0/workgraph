//! The two ends of a shift that concern the installed binary (design §7.3 step 2, §12.5):
//! self-update at the start (when origin/main of the factory moved to a SHA whose CI passed,
//! install it and re-exec), and the canary at both ends (a crashed last shift, or this one).
//! Only a process running the installed `~/.workgraph/bin/current/wg` updates itself; a binary
//! run from anywhere else (a checkout, the tests) never does.

use crate::config::Config;
use crate::gh::Gh;
use crate::journal::Journal;
use crate::selfimprove::install;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Is this process the installed `current` binary?
pub fn installed(dirs: &Dirs) -> bool {
    let cur = dirs.bin().join("current").join("wg").canonicalize();
    let exe = std::env::current_exe().and_then(|p| p.canonicalize());
    matches!((cur, exe), (Ok(c), Ok(e)) if c == e)
}

/// The `bin/` directory a main SHA installs to.
fn dir_of(sha: &str) -> String {
    format!("wg-{}", &sha[..12])
}

/// Installs origin/main when it moved and its CI passed; the path to re-exec, if it did.
pub fn update(dirs: &Dirs, cfg: &Config, gh: &Gh, j: &Journal) -> Result<Option<PathBuf>, String> {
    if !cfg.self_improve.enabled {
        return Ok(None);
    }
    if !installed(dirs) {
        j.append(
            "self_update",
            json!({ "skipped": "not running the installed binary" }),
        )?;
        return Ok(None);
    }
    let Some(repo) = cfg.repo(&cfg.self_improve.repo) else {
        return Ok(None);
    };
    let path = Path::new(&repo.path);
    crate::git::fetch(path)?;
    let main = crate::git::git(path, &["rev-parse", "origin/main"], &[])?;
    // What `current` points at decides, not the ledger's last install: a rollback or a drill can
    // leave those apart, and re-exec'ing an install that changed nothing would never end.
    if install::sha_ok(&main) && install::link(dirs, "current") == Some(dir_of(&main)) {
        return Ok(None);
    }
    match install::install(dirs, gh, &cfg.self_improve.repo, &main) {
        Ok(v) if v["already"] == true => Ok(None),
        Ok(v) => {
            j.append("self_update", json!({ "installed": v }))?;
            Ok(Some(dirs.bin().join("current").join("wg")))
        }
        Err(why) => {
            // CI still running, a denylisted SHA, no artifact yet: this shift keeps its binary.
            j.append("self_update", json!({ "sha": main, "not_installed": why }))?;
            Ok(None)
        }
    }
}

/// The canary over shift `id`; a rollback is journalled in `j`. True when it rolled back.
pub fn canary(dirs: &Dirs, cfg: &Config, gh: &Gh, id: &str, j: &Journal) -> Result<bool, String> {
    let v: Value = crate::cli::self_::canary(dirs, cfg, gh, id)?;
    if v["healthy"] == false {
        j.append(
            "rolled_back",
            json!({ "shift": id, "rollback": v["rollback"] }),
        )?;
        return Ok(true);
    }
    Ok(false)
}
