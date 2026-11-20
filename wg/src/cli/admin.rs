//! Owner-only configuration: `wg config allow-check`, `wg config pin-haruspex`, `wg memory import`
//! and the `apiKeyHelper` entry point `wg auth key`. Each runs only after the owner's yes in chat;
//! none runs inside an agent.

use super::args::{not_an_agent, parse};
use crate::config::{CheckEntry, Pin};
use crate::state_dirs::{private_dir, Dirs};
use crate::{git, memory_store};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

pub fn config(args: &[String]) -> Result<Value, String> {
    not_an_agent("wg config")?;
    match args.first().map(String::as_str) {
        Some("allow-check") => allow_check(&args[1..]),
        Some("pin-haruspex") => pin_haruspex(&args[1..]),
        _ => Err("usage: wg config allow-check | pin-haruspex".into()),
    }
}

/// Appends an approved check; a later entry for the same repo and name overrides an earlier one,
/// so the owner's file keeps its comments and history.
fn allow_check(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &[])?;
    a.only(&["repo", "name", "cmd", "factory-cmd"])?;
    let entry = CheckEntry {
        repo: a.need("repo")?.to_string(),
        name: a.need("name")?.to_string(),
        project: a.need("cmd")?.to_string(),
        factory: a.need("factory-cmd")?.to_string(),
    };
    let dirs = Dirs::from_env()?;
    let path = dirs.config();
    let existing = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    #[derive(serde::Serialize)]
    struct One<'a> {
        check: [&'a CheckEntry; 1],
    }
    let block = toml::to_string(&One { check: [&entry] }).map_err(|e| e.to_string())?;
    let next = format!(
        "{existing}{}\n{block}",
        if existing.ends_with('\n') || existing.is_empty() {
            ""
        } else {
            "\n"
        }
    );
    crate::config::parse(&next)?;
    write_config(&path, &next)?;
    Ok(
        json!({ "allowed": true, "repo": entry.repo, "name": entry.name, "project": entry.project, "factory": entry.factory }),
    )
}

fn write_config(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut f =
        std::fs::File::create(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// Pins the Haruspex CLI the kernel and the agents use. Refuses a path inside the current repo or
/// the agents' work tree.
fn pin_haruspex(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &[])?;
    a.only(&["cli"])?;
    let cli = a.need("cli")?;
    let dirs = Dirs::from_env()?;
    let mut forbidden = vec![dirs.work(), dirs.state()];
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(top) = git::toplevel(&cwd) {
            forbidden.push(top.into());
        }
    }
    let canon = crate::haruspex::cli::check_location(cli, &forbidden)?;
    let pin = Pin {
        cli: canon.to_string_lossy().into_owned(),
        sha256: crate::haruspex::cli::sha256_file(&canon)?,
    };
    let path = dirs.config();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut table: toml::Table = toml::from_str(&text).map_err(|e| format!("factory.toml: {e}"))?;
    table.insert(
        "haruspex".into(),
        toml::Value::try_from(&pin).map_err(|e| e.to_string())?,
    );
    let next = toml::to_string(&table).map_err(|e| e.to_string())?;
    crate::config::parse(&next)?;
    write_config(&path, &next)?;
    Ok(json!({ "pinned": true, "cli": pin.cli, "sha256": pin.sha256 }))
}

pub fn memory(args: &[String]) -> Result<Value, String> {
    if args.first().map(String::as_str) != Some("import") || args.len() != 1 {
        return Err("usage: wg memory import".into());
    }
    let repo = super::run::repo_root()?;
    let dirs = Dirs::from_env()?;
    let target = dirs.memory(&git::slug(&repo));
    memory_store::import(&repo.join(".claude/work-memory.json"), &target)
}

/// `wg auth key <provider>`: the key on stdout and nowhere else (D5). Its errors go to stderr.
pub fn auth(args: &[String]) -> Result<String, String> {
    let provider = match args {
        [k, p] if k == "key" => p,
        _ => return Err("usage: wg auth key <provider>".into()),
    };
    // No WG_AGENT tripwire here: Claude Code runs this helper inside the child's environment. The
    // guard and the disallowed list keep the child's own Bash from calling it (design §14).
    let dirs = Dirs::from_env()?;
    let cfg = super::run::load_config(&dirs)?;
    crate::claude::auth::key(&cfg, provider, &crate::claude::auth::run_fixed)
}
