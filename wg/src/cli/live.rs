//! `wg doctor --live`: one small haiku call on the default provider, with no tools, in a throwaway
//! directory, through the same argv, env, settings and guard as a real run.

use crate::claude::argv::{argv, Input};
use crate::claude::auth::{env, plugin_profile, settings, EnvInput};
use crate::claude::envelope::classify;
use crate::claude::policy::ALWAYS_DENIED;
use crate::config::Config;
use crate::exec::proc::{Proc, RealProc, Spawn};
use crate::guard::hook::Manifest;
use crate::state_dirs::{private_dir, Dirs};
use serde_json::json;
use std::time::Duration;
use wg_factory::api::Tier;

pub fn probe(cfg: &Config, dirs: &Dirs) -> Result<String, String> {
    let provider = cfg
        .auth
        .get(&cfg.auth.default)
        .ok_or("the default provider is not defined")?;
    let id = format!("doctor-{}", crate::tokens_store::random_hex(4)?);
    let dir = dirs.run(&id);
    let tmp = dirs.run_tmp(&id);
    private_dir(&dir)?;
    private_dir(&tmp)?;
    let manifest_path = dir.join("doctor.manifest.json");
    let manifest = Manifest {
        label: "doctor".into(),
        log: dir.join("guard.jsonl").to_string_lossy().into_owned(),
        worktree: tmp.to_string_lossy().into_owned(),
        read_roots: vec![],
        write_roots: vec![],
        secret_roots: vec![],
        tools: vec![],
        skills: vec![],
        bash: Default::default(),
    };
    let write = |p: &std::path::Path, v: &serde_json::Value| {
        std::fs::write(p, v.to_string()).map_err(|e| format!("cannot write {}: {e}", p.display()))
    };
    write(
        &manifest_path,
        &serde_json::to_value(&manifest).expect("serializes"),
    )?;
    let wg = std::env::current_exe().map_err(|e| format!("cannot find wg itself: {e}"))?;
    let settings_path = dir.join("doctor.settings.json");
    let plugins = plugin_profile(&dirs.home, &cfg.keep_plugins);
    write(
        &settings_path,
        &settings(provider, &wg, &manifest_path, &plugins),
    )?;
    let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } },
                         "required": ["ok"], "additionalProperties": false });
    let denied: Vec<String> = ALWAYS_DENIED.iter().map(|s| s.to_string()).collect();
    let raw = RealProc.run(&Spawn {
        label: "doctor".into(),
        program: cfg.claude_bin.clone(),
        argv: argv(&Input {
            model: provider.model(Tier::Haiku),
            effort: Some(wg_factory::api::Effort::Low),
            schema: &schema,
            max_turns: 2,
            runaway_usd: cfg.runaway_per_call_usd,
            settings: &settings_path,
            allowed: &[],
            disallowed: &denied,
            preamble: "",
        }),
        env: env(
            provider,
            &EnvInput {
                home: &dirs.home,
                user: &std::env::var("USER").unwrap_or_default(),
                tmpdir: &tmp,
                run: &id,
                manifest: &manifest_path,
            },
        ),
        cwd: tmp.clone(),
        stdin: "Reply with ok = true.".into(),
        timeout: Duration::from_secs(120),
    });
    std::fs::write(dir.join("doctor.envelope.json"), &raw.stdout)
        .map_err(|e| format!("cannot keep the envelope: {e}"))?;
    let e = classify(raw.exit, raw.timed_out, &raw.stdout)
        .map_err(|f| format!("{:?}: {}", f.class, f.detail))?;
    if e.structured.as_ref().and_then(|s| s["ok"].as_bool()) != Some(true) {
        return Err("the reply had no structured_output with ok = true".into());
    }
    Ok(format!(
        "{} answered on {} ({}), cost {}",
        provider.model(Tier::Haiku),
        provider.id,
        e.model_used.unwrap_or_default(),
        e.cost_micros
            .map_or("unknown".into(), |c| format!("${:.4}", c as f64 / 1e6))
    ))
}
