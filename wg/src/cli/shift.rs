//! `wg shift run|resume|status|report|recover|gc|retry-release|schedule-print`, `wg decide`, `wg approve` and
//! `wg approval-summary` (design §3.1, §7, §9.5). Each prints one JSON line.

use super::args::{not_an_agent, parse};
use crate::gh::Gh;
use crate::shift::{admin, owner, report, schedule};
use crate::state_dirs::Dirs;
use serde_json::Value;

fn setup() -> Result<(Dirs, crate::config::Config), String> {
    let dirs = Dirs::from_env()?;
    let cfg = super::run::load_config(&dirs)?;
    Ok((dirs, cfg))
}

fn gh(dirs: &Dirs, cfg: &crate::config::Config) -> Gh {
    Gh::new(&cfg.gh_bin, dirs.state().join("gh"))
}

fn number(s: &str, what: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("--{what} must be a positive number"))
}

pub fn shift(args: &[String]) -> Result<Value, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let (dirs, cfg) = setup()?;
    match sub {
        "run" | "resume" | "recover" | "gc" | "retry-release" => {
            not_an_agent(&format!("wg shift {sub}"))?
        }
        _ => {}
    }
    let none = |r: &[String]| parse(r, &[]).and_then(|a| a.only(&[]));
    match (sub, rest) {
        ("run", r) => none(r).and_then(|_| crate::shift::run::run(&dirs, &cfg)),
        ("resume", [id]) => admin::resume(&dirs, &cfg, id),
        ("status", r) => none(r).and_then(|_| admin::status(&dirs)),
        ("report", []) => match report::latest(&dirs)? {
            Some(id) => report::write(&dirs, &id),
            None => Err("no shift has run yet".into()),
        },
        ("report", [id]) => {
            crate::ids::run_id(id).map_err(|_| format!("{id:?} is not a shift id"))?;
            report::write(&dirs, id)
        }
        ("recover", r) => none(r).and_then(|_| admin::recover(&dirs, &cfg, &gh(&dirs, &cfg))),
        ("gc", r) => none(r).and_then(|_| admin::gc(&dirs, &cfg, &gh(&dirs, &cfg))),
        ("retry-release", r) => {
            let a = parse(r, &[])?;
            a.only(&["repo", "issue"])?;
            crate::shift::release::retry(&dirs, &cfg, a.need("repo")?, number(a.need("issue")?, "issue")?)
        }
        ("schedule-print", r) => none(r).map(|_| schedule::print(&dirs, &cfg)),
        _ => Err("usage: wg shift run | resume <id> | status | report [id] | recover | gc | retry-release --repo <r> --issue <n> | schedule-print".into()),
    }
}

pub fn decide(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &[])?;
    a.only(&["repo", "issue", "decisions-file"])?;
    not_an_agent("wg decide")?;
    let (dirs, cfg) = setup()?;
    let repo = a.need("repo")?;
    owner::decide(
        &dirs,
        &cfg,
        &gh(&dirs, &cfg),
        repo,
        number(a.need("issue")?, "issue")?,
        a.need("decisions-file")?,
    )
}

pub fn approve(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &[])?;
    a.only(&["repo", "pr", "by"])?;
    not_an_agent("wg approve")?;
    let (dirs, cfg) = setup()?;
    owner::approve(
        &dirs,
        &cfg,
        &gh(&dirs, &cfg),
        a.need("repo")?,
        number(a.need("pr")?, "pr")?,
        a.need("by")?,
    )
}

pub fn approval_summary(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &[])?;
    a.only(&["repo", "pr"])?;
    let (dirs, cfg) = setup()?;
    owner::summary(
        &dirs,
        &cfg,
        &gh(&dirs, &cfg),
        a.need("repo")?,
        number(a.need("pr")?, "pr")?,
    )
}
