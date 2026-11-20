//! `recall`, `check`, `save`, `finalize`: the flags, parser and JSON of wg 0.3, byte for byte.
//! The parser is hand-rolled on purpose: it accepts hyphen-leading values (`--cmd -x`) and every
//! error is the one JSON line, which a generic argument parser would not give.

use crate::exec::run::run_shell;
use crate::git::dirty;
use crate::haruspex::finalize::{finalize_chain, Finalize};
use crate::memory;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

/// Runs one legacy subcommand. `args` excludes the program name.
pub fn run(args: &[String]) -> Result<Value, String> {
    match args.first().map(String::as_str) {
        Some("recall") => recall(),
        Some("check") => check(&args[1..]),
        Some("save") => save(),
        Some("finalize") => finalize(&args[1..]),
        _ => Err("usage: wg recall | check | save | finalize".to_string()),
    }
}

/// `--name value` pairs in order; a flag given twice keeps every value.
fn flags(args: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let name = k
            .strip_prefix("--")
            .ok_or(format!("unexpected argument: {k}"))?;
        let v = it.next().ok_or(format!("--{name} needs a value"))?;
        out.push((name.to_string(), v.clone()));
    }
    Ok(out)
}

fn last(f: &[(String, String)], name: &str) -> Option<String> {
    f.iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn one(f: &[(String, String)], name: &str) -> Result<String, String> {
    last(f, name).ok_or(format!("--{name} is required"))
}

fn recall() -> Result<Value, String> {
    let dirty = dirty()?;
    match memory::read()? {
        Some(content) => Ok(json!({ "exists": true, "content": content, "dirty": dirty })),
        None => Ok(json!({ "exists": false, "dirty": dirty })),
    }
}

fn check(args: &[String]) -> Result<Value, String> {
    let f = flags(args)?;
    let nonce = one(&f, "nonce")?;
    let secs: u64 = match last(&f, "timeout-secs") {
        Some(v) => v
            .parse()
            .map_err(|_| "--timeout-secs must be a whole number".to_string())?,
        None => 600,
    };
    let results: Vec<Value> = f
        .iter()
        .filter(|(k, _)| k == "cmd")
        .map(|(_, c)| {
            let (code, out) = run_shell(c, Duration::from_secs(secs));
            json!({ "cmd": c, "exitCode": code, "outputTail": out })
        })
        .collect();
    Ok(json!({ "nonce": nonce, "results": results, "changedFiles": dirty()? }))
}

fn save() -> Result<Value, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    let memory: Value =
        serde_json::from_str(&input).map_err(|e| format!("stdin is not JSON: {e}"))?;
    memory::save(&memory)
}

/// Run only after the user approved the build gate and the commit. Stops at the first failing step.
fn finalize(args: &[String]) -> Result<Value, String> {
    let f = flags(args)?;
    let (task, by, cli) = (one(&f, "task")?, one(&f, "by")?, one(&f, "cli")?);
    let (message, evidence_message) = (one(&f, "message")?, one(&f, "evidence-message")?);
    let exclude: HashSet<String> = match last(&f, "exclude-json") {
        Some(v) => serde_json::from_str::<Vec<String>>(&v)
            .map_err(|e| format!("--exclude-json must be a JSON array of paths: {e}"))?
            .into_iter()
            .collect(),
        None => HashSet::new(),
    };
    if !Path::new(&cli).is_absolute() {
        return Err("--cli must be an absolute path".into());
    }
    let staged: Vec<String> = dirty()?
        .into_iter()
        .filter(|p| !exclude.contains(p))
        .collect();
    if staged.is_empty() {
        return Err("nothing to commit outside the excluded paths".into());
    }
    Ok(finalize_chain(&Finalize {
        task,
        by,
        cli,
        message,
        evidence_message,
        staged,
    }))
}
