//! Per-call environment and `--settings` from a provider profile, and `wg auth key` (design §5.1,
//! D5, D6). No key ever reaches an environment, argv, file or log: a key-bearing profile's settings
//! name `apiKeyHelper` = `<wg> auth key <provider>`, whose stdout is its pipe to `claude`.

use super::providers::{Kind, Provider};
use crate::config::Config;
use serde_json::{json, Map, Value};
use std::path::Path;

/// The fixed PATH every child gets.
pub const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

pub struct EnvInput<'a> {
    pub home: &'a Path,
    pub user: &'a str,
    pub tmpdir: &'a Path,
    pub run: &'a str,
    pub manifest: &'a Path,
}

/// The cleared environment plus the allowlist; `ANTHROPIC_BASE_URL` only for a gateway.
pub fn env(p: &Provider, i: &EnvInput) -> Vec<(String, String)> {
    let mut e: Vec<(String, String)> = vec![
        ("HOME".into(), i.home.to_string_lossy().into_owned()),
        ("USER".into(), i.user.into()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("TERM".into(), "dumb".into()),
        ("PATH".into(), PATH.into()),
        ("TMPDIR".into(), i.tmpdir.to_string_lossy().into_owned()),
        ("WG_RUN".into(), i.run.into()),
        (
            "WG_GUARD_MANIFEST".into(),
            i.manifest.to_string_lossy().into_owned(),
        ),
        ("WG_AGENT".into(), "1".into()),
        ("ARGUS_SSH_HOST".into(), "oreochiserver-sim".into()),
    ];
    if let Kind::Gateway { base_url } = &p.kind {
        e.push(("ANTHROPIC_BASE_URL".into(), base_url.clone()));
    }
    e
}

/// Single-quotes a path for the hook and helper command strings Claude Code runs through a shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The per-call settings: the guard hook, path denials, the plugin profile, and `apiKeyHelper` on
/// key-bearing profiles.
pub fn settings(p: &Provider, wg: &Path, manifest: &Path, plugins: &[(String, bool)]) -> Value {
    let wg = shell_quote(&wg.to_string_lossy());
    let guard = format!(
        "{wg} guard --manifest {}",
        shell_quote(&manifest.to_string_lossy())
    );
    let deny = [
        "Read(~/.ssh/**)",
        "Read(~/.workgraph/**)",
        "Read(~/.config/**)",
        "Edit(.haruspex/state.json)",
        "Write(.haruspex/state.json)",
        "Edit(.haruspex/evidence/**)",
        "Write(.haruspex/evidence/**)",
        "Edit(.haruspex/bin/**)",
        "Write(.haruspex/bin/**)",
        "Edit(.haruspex/templates/**)",
        "Write(.haruspex/templates/**)",
        "Edit(.haruspex/managed/**)",
        "Write(.haruspex/managed/**)",
        "Edit(.git/**)",
        "Write(.git/**)",
    ];
    let enabled: Map<String, Value> = plugins
        .iter()
        .map(|(name, on)| (name.clone(), Value::Bool(*on)))
        .collect();
    let mut s = json!({
        "hooks": { "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": guard, "timeout": 60 }] }] },
        "permissions": { "deny": deny },
        "enabledPlugins": enabled,
    });
    if p.keychain_service.is_some() {
        s["apiKeyHelper"] = json!(format!("{wg} auth key {}", p.id));
    }
    s
}

/// Every plugin Claude Code knows for this user, each marked kept or turned off. `keep` holds
/// plugin names before the `@`.
pub fn plugin_profile(home: &Path, keep: &[String]) -> Vec<(String, bool)> {
    let read = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    };
    let mut names: Vec<String> = Vec::new();
    if let Some(v) = read(&home.join(".claude/plugins/installed_plugins.json")) {
        let map = v.get("plugins").unwrap_or(&v);
        names.extend(map.as_object().into_iter().flat_map(|m| m.keys().cloned()));
    }
    if let Some(v) = read(&home.join(".claude/settings.json")) {
        names.extend(
            v["enabledPlugins"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.keys().cloned()),
        );
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|n| {
            let short = n.split('@').next().unwrap_or("").to_string();
            let kept = keep.contains(&short);
            (n, kept)
        })
        .collect()
}

/// Runs a fixed argv and returns its stdout (a seam for the fake keychain in tests).
pub type RunFixed<'a> = &'a dyn Fn(&str, &[&str]) -> Result<String, String>;

/// `wg auth key <provider>`: the key from the profile's keychain item, for stdout only.
pub fn key(cfg: &Config, provider: &str, run: RunFixed) -> Result<String, String> {
    let p = cfg
        .auth
        .get(provider)
        .ok_or_else(|| format!("no provider {provider:?} in factory.toml"))?;
    let service = p
        .keychain_service
        .as_deref()
        .ok_or_else(|| format!("provider {provider} has no keychain_service"))?;
    let key = run(
        &cfg.security_bin,
        &["find-generic-password", "-s", service, "-w"],
    )?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(format!("the keychain item for {provider} is empty"));
    }
    Ok(key)
}

/// Runs a fixed argv with a cleared environment and returns its stdout; never logs it.
pub fn run_fixed(program: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", PATH)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{program} failed ({})", out.status));
    }
    String::from_utf8(out.stdout).map_err(|_| format!("{program} printed non-UTF-8 output"))
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
