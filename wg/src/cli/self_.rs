//! `wg self gate|install|rollback|pause|resume|status` and `wg eval replay|live|capture|arm`
//! (design §3.1, §12). Each prints one JSON line. The owner-only ones refuse inside an agent.

use super::args::{not_an_agent, parse};
use crate::config::Config;
use crate::gh::Gh;
use crate::selfimprove::{
    capture, eval_store, gate, install, ledger::Ledger, rate, replay_check, rollback, state,
};
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn setup() -> Result<(Dirs, Config), String> {
    let dirs = Dirs::from_env()?;
    let cfg = super::run::load_config(&dirs)?;
    Ok((dirs, cfg))
}

pub fn gh(dirs: &Dirs, cfg: &Config) -> Gh {
    Gh::new(&cfg.gh_bin, dirs.state().join("gh"))
}

fn number(s: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "--pr must be a positive number".into())
}

/// The owner's checkout of the factory repo, from its `[[repo]]` entry.
pub fn checkout(cfg: &Config) -> Result<PathBuf, String> {
    cfg.repo(&cfg.self_improve.repo)
        .map(|r| PathBuf::from(&r.path))
        .ok_or_else(|| {
            format!(
                "{} is not a [[repo]] in factory.toml",
                cfg.self_improve.repo
            )
        })
}

pub fn run_gate(
    dirs: &Dirs,
    cfg: &Config,
    gh: &Gh,
    pr: u64,
    merges_this_shift: u32,
) -> Result<gate::Outcome, String> {
    let co = checkout(cfg)?;
    let eval = eval_store::StoreEval {
        dirs,
        exe: std::env::current_exe().map_err(|e| format!("cannot find wg itself: {e}"))?,
    };
    gate::gate(
        &gate::Env {
            dirs,
            cfg,
            gh,
            checkout: &co,
            merges_this_shift,
            eval: &eval,
        },
        pr,
    )
}

fn status(dirs: &Dirs, cfg: &Config) -> Result<Value, String> {
    let st = state::load(&dirs.self_state())?;
    let ledger = Ledger::at(&dirs.self_ledger());
    let decisions = ledger.of_kind("decision")?;
    let rules = crate::selfimprove::protected::compiled();
    Ok(json!({
        "enabled": cfg.self_improve.enabled, "paused": st.paused, "pause_reason": st.pause_reason,
        "merge_enabled": st.merge_enabled, "shadow_decisions": rate::shadow_count(&ledger)?,
        "shadow_needed": rules.shadow_prs, "merges": ledger.merges()?.len(), "denylist": st.denylist,
        "current": install::link(dirs, "current"), "previous": install::link(dirs, "previous"),
        "recent_decisions": decisions.iter().rev().take(10).cloned().collect::<Vec<_>>(),
    }))
}

pub fn self_(args: &[String]) -> Result<Value, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    if sub != "status" {
        not_an_agent(&format!("wg self {sub}"))?;
    }
    let (dirs, cfg) = setup()?;
    let g = gh(&dirs, &cfg);
    match sub {
        "gate" => {
            let a = parse(rest, &[])?;
            a.only(&["pr"])?;
            let o = run_gate(&dirs, &cfg, &g, number(a.need("pr")?)?, 0)?;
            Ok(json!({ "outcome": o.name(), "reason": o.reason() }))
        }
        "install" => {
            let a = parse(rest, &[])?;
            a.only(&["sha"])?;
            install::install(&dirs, &g, &cfg.self_improve.repo, a.need("sha")?)
        }
        "rollback" => {
            let a = parse(rest, &[])?;
            a.only(&["reason"])?;
            let co = checkout(&cfg).ok();
            rollback::rollback(
                &rollback::Env { dirs: &dirs, gh: &g, repo: &cfg.self_improve.repo, checkout: co.as_deref() },
                a.get("reason").unwrap_or("the owner rolled back"),
            )
        }
        "pause" => {
            let a = parse(rest, &[])?;
            a.only(&["reason"])?;
            let st = state::load(&dirs.self_state())?.paused_by(a.get("reason").unwrap_or("paused by the owner"));
            state::save(&dirs.self_state(), &st)?;
            Ledger::at(&dirs.self_ledger()).append("paused", json!({ "reason": st.pause_reason }))?;
            Ok(json!({ "paused": true }))
        }
        "resume" => {
            let a = parse(rest, &["enable-merge"])?;
            a.only(&["enable-merge"])?;
            let ledger = Ledger::at(&dirs.self_ledger());
            let rules = crate::selfimprove::protected::compiled();
            let st = state::load(&dirs.self_state())?.resumed(
                a.on("enable-merge"),
                rate::shadow_count(&ledger)?,
                rules.shadow_prs,
                crate::journal::now(),
            )?;
            state::save(&dirs.self_state(), &st)?;
            ledger.append("resumed", json!({ "merge_enabled": st.merge_enabled }))?;
            Ok(json!({ "paused": false, "merge_enabled": st.merge_enabled }))
        }
        "status" => status(&dirs, &cfg),
        _ => Err("usage: wg self gate --pr N | install --sha S | rollback [--reason R] | pause [--reason R] | resume [--enable-merge] | status".into()),
    }
}

fn live(dirs: &Dirs, cfg: &Config, gh: &Gh, pr: u64) -> Result<Value, String> {
    use crate::selfimprove::{diff, github};
    let co = checkout(cfg)?;
    let p = github::self_pr(gh, &cfg.self_improve.repo, pr)?;
    diff::fetch(&co, pr, &p.head)?;
    let d = diff::diff(&co, &diff::rules(&co)?, &p.head)?;
    let overlay = dirs
        .eval_work()
        .join(format!("live-{pr}-{}", &p.head[..12]));
    diff::write_overlay(&co, &p.head, &diff::asset_changes(&d.changes), &overlay)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let r = gate::LiveEval::run(&eval_store::StoreEval { dirs, exe }, pr, &p.head, &overlay)?;
    Ledger::at(&dirs.self_ledger()).append(
        "eval",
        json!({ "pr": pr, "head": p.head, "passed": r.passed, "reason": r.reason, "spend": r.spend, "by": "wg eval live" }),
    )?;
    Ok(json!({ "passed": r.passed, "reason": r.reason }))
}

pub fn eval(args: &[String]) -> Result<Value, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    match sub {
        "replay" => {
            // CI's replay-eval job: base's cases and manifest, this build's engine. No config.
            let a = parse(rest, &[])?;
            a.only(&["cases", "manifest"])?;
            let cases = Path::new(a.get("cases").unwrap_or("eval/replay"));
            let text = std::fs::read_to_string(a.get("manifest").unwrap_or("eval/manifest.toml"))
                .map_err(|e| format!("cannot read the manifest: {e}"))?;
            let scratch = std::env::temp_dir().join(format!("wg-replay-{}", std::process::id()));
            let out = replay_check::suite(cases, &replay_check::manifest(&text)?, &scratch)?;
            let _ = std::fs::remove_dir_all(&scratch);
            if out["failed"] != 0 {
                return Err(format!("replay failed: {out}"));
            }
            Ok(out)
        }
        "live" | "capture" | "arm" => {
            not_an_agent(&format!("wg eval {sub}"))?;
            let (dirs, cfg) = setup()?;
            let g = gh(&dirs, &cfg);
            match sub {
                "live" => {
                    let a = parse(rest, &[])?;
                    a.only(&["pr"])?;
                    live(&dirs, &cfg, &g, number(a.need("pr")?)?)
                }
                "capture" => {
                    let a = parse(rest, &[])?;
                    a.only(&["pr", "expect"])?;
                    capture::capture(&dirs, &cfg, &g, a.need("pr")?, a.get("expect").unwrap_or("done"))
                }
                _ => {
                    let a = parse(rest, &[])?;
                    a.only(&["fixture", "arm", "overlay", "tag"])?;
                    let arm = a.need("arm")?;
                    if !["base", "candidate"].contains(&arm) {
                        return Err("--arm is base or candidate".into());
                    }
                    let tag = a.need("tag")?;
                    crate::ids::gh_label(tag).map_err(|_| "--tag is a short lowercase name".to_string())?;
                    eval_store::arm(&dirs, &cfg, Path::new(a.need("fixture")?), arm, a.get("overlay").map(Path::new), tag)
                }
            }
        }
        _ => Err("usage: wg eval replay [--cases DIR --manifest F] | live --pr N | capture --pr URL [--expect done|needs_decision]".into()),
    }
}
