//! `wg doctor [--live]`: whether this Mac can run the factory, and the user-settings refusal the
//! runner shares (issue #10, ADR-001 §10).

use super::args::parse;
use crate::claude::providers::Kind;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::path::Path;

/// `ask` rules the work skill itself offers to install (design §9.4). Under `-p` an ask is a deny
/// (ADR-001 §14), so these only narrow what children may do.
const SKILL_ASK_RULES: [&str; 4] = [
    "Bash(*wg finalize*)",
    "Bash(*wg approve*)",
    "Bash(*wg self*)",
    "Bash(*wg config*)",
];

/// `Some(reason)` when user-scope settings would widen or reinterpret every role's allowlist:
/// with `--setting-sources user`, a user `permissions.allow` entry merges into each child's, and a
/// `defaultMode` changes what `dontAsk` means.
pub fn user_settings_problem(path: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let p = &v["permissions"];
    let mut bad = Vec::new();
    if p["allow"].as_array().is_some_and(|a| !a.is_empty()) {
        bad.push("permissions.allow");
    }
    if !p["defaultMode"].is_null() {
        bad.push("permissions.defaultMode");
    }
    let foreign_ask = p["ask"].as_array().is_some_and(|a| {
        a.iter()
            .any(|r| !SKILL_ASK_RULES.contains(&r.as_str().unwrap_or("")))
    });
    if foreign_ask {
        bad.push("permissions.ask (other than the work skill's wg rules)");
    }
    Ok((!bad.is_empty()).then(|| {
        format!(
            "{} sets {}; with --setting-sources user that changes every factory child's permissions. Move it to a project or local settings file first.",
            path.display(),
            bad.join(", ")
        )
    }))
}

/// Names (never values) of user-settings `env` entries that look like credentials.
fn credential_env(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["env"].as_object().cloned())
        .map(|m| {
            m.keys()
                .filter(|k| crate::exec::env::secret(k))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

pub fn doctor(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &["live"])?;
    a.only(&["live"])?;
    let dirs = Dirs::from_env()?;
    let mut checks: Vec<Value> = Vec::new();
    let mut add = |name: &str, ok: bool, level: &str, detail: String| {
        checks.push(json!({ "check": name, "ok": ok, "level": level, "detail": detail }));
    };
    let cfg = match super::run::load_config(&dirs) {
        Ok(c) => {
            add(
                "factory.toml",
                true,
                "error",
                dirs.config().display().to_string(),
            );
            Some(c)
        }
        Err(e) => {
            add("factory.toml", false, "error", e);
            None
        }
    };
    match user_settings_problem(&dirs.claude_settings()) {
        Ok(None) => add(
            "user permissions",
            true,
            "error",
            "no allow, ask or defaultMode".into(),
        ),
        Ok(Some(p)) | Err(p) => add("user permissions", false, "error", p),
    }
    let creds = credential_env(&dirs.claude_settings());
    add(
        "user settings env",
        creds.is_empty(),
        "warning",
        if creds.is_empty() {
            "no credential-like names".into()
        } else {
            format!(
                "user settings env names credentials ({}); children inherit them",
                creds.join(", ")
            )
        },
    );
    let api_key_set = std::env::var_os("ANTHROPIC_API_KEY").is_some();
    add(
        "ANTHROPIC_API_KEY",
        !api_key_set,
        "warning",
        if api_key_set {
            "set in wg's environment; it is ignored and never reaches a child".into()
        } else {
            "not set".into()
        },
    );
    if let Some(cfg) = &cfg {
        add(
            "claude",
            Path::new(&cfg.claude_bin).exists(),
            "error",
            cfg.claude_bin.clone(),
        );
        let pinned = match &cfg.haruspex {
            None => Err("no [haruspex] pin: wg config pin-haruspex --cli <abs>".to_string()),
            Some(p) => crate::haruspex::cli::sha256_file(Path::new(&p.cli)).and_then(|h| {
                if h == p.sha256 {
                    Ok(p.cli.clone())
                } else {
                    Err(format!("{} no longer matches its pin", p.cli))
                }
            }),
        };
        match pinned {
            Ok(d) => add("haruspex pin", true, "error", d),
            Err(e) => add("haruspex pin", false, "error", e),
        }
        match crate::pipelines::load(crate::assets::DEV_PIPELINE, cfg.max_calls_per_task) {
            Ok(_) => add(
                "dev pipeline",
                true,
                "error",
                format!("fits {} calls", cfg.max_calls_per_task),
            ),
            Err(e) => add("dev pipeline", false, "error", e),
        }
        // The load check per repo (design §5.2): each chain's worst case must fit the call cap.
        let assets = crate::assets::Assets::compiled();
        for r in &cfg.repos {
            let release = vec!["release".to_string()];
            let lines = std::iter::once(&r.pipelines).chain(r.release.then_some(&release));
            for names in lines {
                let view = crate::pipelines::RepoView::of(r);
                match crate::pipelines::chain(&assets, names, &view, cfg.max_calls_per_task) {
                    Ok(c) => add(
                        &format!("chain {} {}", r.name, names.join("+")),
                        true,
                        "error",
                        format!(
                            "worst case {} of {} calls",
                            crate::pipelines::worst_case(&c),
                            cfg.max_calls_per_task
                        ),
                    ),
                    Err(e) => add(
                        &format!("chain {} {}", r.name, names.join("+")),
                        false,
                        "error",
                        e,
                    ),
                }
            }
        }
        for r in &cfg.repos {
            if let Some(n) = crate::runtime_verify::evidence_checks(cfg, r).next() {
                let res = crate::runtime_verify::verify_service(r.factory_compose.as_deref());
                let ok = res.is_ok();
                let detail = res.map_or_else(|e| e, |()| format!("defined (evidence check {n})"));
                add(&format!("verify service {}", r.name), ok, "error", detail);
            }
        }
        for p in &cfg.auth.providers {
            let note = match (&p.kind, &p.keychain_service) {
                (Kind::Gateway { .. }, _) => {
                    Some("gateway profiles are unconfirmed (ADR-001 §9); expect Invalid calls")
                }
                (_, Some(_)) => Some("key-bearing profiles wait on child isolation (issue #9)"),
                _ => None,
            };
            if let Some(n) = note {
                add(&format!("provider {}", p.id), false, "warning", n.into());
            }
        }
        if a.on("live") {
            match super::live::probe(cfg, &dirs) {
                Ok(d) => add("live call", true, "error", d),
                Err(e) => add("live call", false, "error", e),
            }
        }
    }
    let ok = checks
        .iter()
        .all(|c| c["ok"] == true || c["level"] == "warning");
    Ok(json!({ "ok": ok, "checks": checks }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str, body: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wg-doctor-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("settings.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn user_permissions_that_widen_or_reinterpret_are_refused() {
        assert_eq!(
            user_settings_problem(Path::new("/nonexistent/s.json")).unwrap(),
            None
        );
        assert_eq!(
            user_settings_problem(&settings("none", r#"{"hooks":{}}"#)).unwrap(),
            None
        );
        assert_eq!(
            user_settings_problem(&settings(
                "deny",
                r#"{"permissions":{"deny":["Read(x)"],"allow":[]}}"#
            ))
            .unwrap(),
            None
        );
        let ok_ask = r#"{"permissions":{"ask":["Bash(*wg finalize*)","Bash(*wg config*)"]}}"#;
        assert_eq!(
            user_settings_problem(&settings("skillask", ok_ask)).unwrap(),
            None
        );
        for (name, body, want) in [
            (
                "allow",
                r#"{"permissions":{"allow":["Bash(rm:*)"]}}"#,
                "permissions.allow",
            ),
            (
                "ask",
                r#"{"permissions":{"ask":["Bash(git push:*)"]}}"#,
                "permissions.ask",
            ),
            (
                "mode",
                r#"{"permissions":{"defaultMode":"acceptEdits"}}"#,
                "defaultMode",
            ),
        ] {
            let got = user_settings_problem(&settings(name, body))
                .unwrap()
                .unwrap();
            assert!(got.contains(want), "{got}");
        }
        assert!(user_settings_problem(&settings("bad", "{")).is_err());
        assert_eq!(
            credential_env(&settings(
                "env",
                r#"{"env":{"ANTHROPIC_API_KEY":"x","EDITOR":"vi"}}"#
            )),
            ["ANTHROPIC_API_KEY"]
        );
    }
}
