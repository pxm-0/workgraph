//! Includes the gateway builder dry run (issue #6, ADR-001 §9): the argv, env and settings `wg`
//! builds for `openrouter` and `litellm` profiles, with no network.

use super::*;
use crate::claude::argv::{argv, Input};
use crate::config::parse;
use std::path::PathBuf;
use wg_factory::api::Tier;

const FILE: &str = r#"
[providers.subscription]
kind = "subscription"
models = { haiku = "haiku", sonnet = "sonnet", opus = "opus" }

[providers.anthropic-api]
kind = "anthropic-api"
keychain_service = "workgraph-anthropic"
models = { haiku = "claude-haiku-5-5", sonnet = "claude-sonnet-5-5", opus = "claude-opus-5-5" }

[providers.openrouter]
kind = "gateway"
base_url = "https://openrouter.ai/api"
keychain_service = "workgraph-openrouter"
models = { haiku = "anthropic/claude-haiku-5.5", sonnet = "openai/gpt-5.1", opus = "google/gemini-3-pro" }

[providers.litellm]
kind = "gateway"
base_url = "http://127.0.0.1:4000"
models = { haiku = "local-small", sonnet = "local-mid", opus = "local-big" }
"#;

fn env_for(p: &Provider) -> Vec<(String, String)> {
    env(
        p,
        &EnvInput {
            home: Path::new("/Users/o"),
            user: "o",
            tmpdir: Path::new("/Users/o/.workgraph-work/tmp/r-1"),
            run: "r-1",
            manifest: Path::new("/Users/o/.workgraph/runs/r-1/guard/1.manifest.json"),
        },
    )
}

fn keyish(name: &str) -> bool {
    let n = name.to_uppercase();
    n.contains("KEY") || n.contains("TOKEN") || n.contains("SECRET") || n == "SSH_AUTH_SOCK"
}

#[test]
fn gateway_dry_run_pins_argv_env_and_settings() {
    let cfg = parse(FILE).unwrap();
    let wg = PathBuf::from("/Users/o/.workgraph/bin/wg-abc/wg");
    let manifest = PathBuf::from("/Users/o/.workgraph/runs/r-1/guard/1.manifest.json");
    for (id, url, model, helper) in [
        (
            "openrouter",
            "https://openrouter.ai/api",
            "openai/gpt-5.1",
            true,
        ),
        ("litellm", "http://127.0.0.1:4000", "local-mid", false),
    ] {
        let p = cfg.auth.get(id).unwrap();
        let e = env_for(p);
        assert!(
            e.contains(&("ANTHROPIC_BASE_URL".into(), url.into())),
            "{id}"
        );
        assert!(!e.iter().any(|(k, _)| keyish(k)), "{id}: {e:?}");
        let s = settings(p, &wg, &manifest, &[]);
        assert_eq!(
            s["apiKeyHelper"].as_str().map(str::to_string),
            helper.then(|| format!("'/Users/o/.workgraph/bin/wg-abc/wg' auth key {id}")),
            "{id}"
        );
        assert_eq!(
            s["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "'/Users/o/.workgraph/bin/wg-abc/wg' guard --manifest '/Users/o/.workgraph/runs/r-1/guard/1.manifest.json'"
        );
        let schema = serde_json::json!({});
        let a = argv(&Input {
            model: p.model(Tier::Sonnet),
            effort: None,
            schema: &schema,
            max_turns: 3,
            runaway_usd: 25,
            settings: Path::new("/s.json"),
            allowed: &[],
            disallowed: &[],
            preamble: "",
        });
        assert_eq!(&a[..5], ["-p", "--output-format", "json", "--model", model]);
        assert!(!a.iter().any(|x| x.contains("sk-")), "no key in argv");
        assert!(!p.first_party(), "gateway outcomes never move tier stats");
    }
}

#[test]
fn first_party_profiles_set_no_base_url_and_only_keyed_ones_a_helper() {
    let cfg = parse(FILE).unwrap();
    let wg = Path::new("/wg");
    let m = Path::new("/m.json");
    let sub = cfg.auth.get("subscription").unwrap();
    assert!(!env_for(sub)
        .iter()
        .any(|(k, _)| k == "ANTHROPIC_BASE_URL" || keyish(k)));
    assert!(settings(sub, wg, m, &[]).get("apiKeyHelper").is_none());
    let api = cfg.auth.get("anthropic-api").unwrap();
    assert!(!env_for(api)
        .iter()
        .any(|(k, _)| k == "ANTHROPIC_BASE_URL" || keyish(k)));
    assert_eq!(
        settings(api, wg, m, &[])["apiKeyHelper"],
        "'/wg' auth key anthropic-api"
    );
    assert_eq!(api.model(Tier::Opus), "claude-opus-5-5");
}

#[test]
fn auth_key_reads_the_named_keychain_item_with_a_fixed_argv() {
    let cfg = parse(FILE).unwrap();
    let seen = std::cell::RefCell::new(Vec::new());
    let fake = |prog: &str, args: &[&str]| {
        seen.borrow_mut().push(format!("{prog} {}", args.join(" ")));
        Ok("sk-ant-SENTINEL\n".to_string())
    };
    assert_eq!(
        key(&cfg, "anthropic-api", &fake).unwrap(),
        "sk-ant-SENTINEL"
    );
    assert_eq!(
        seen.borrow()[0],
        "/usr/bin/security find-generic-password -s workgraph-anthropic -w"
    );
    assert!(key(&cfg, "subscription", &fake)
        .unwrap_err()
        .contains("no keychain_service"));
    assert!(key(&cfg, "nope", &fake)
        .unwrap_err()
        .contains("no provider"));
    let empty = |_: &str, _: &[&str]| Ok(" \n".to_string());
    assert!(key(&cfg, "anthropic-api", &empty)
        .unwrap_err()
        .contains("empty"));
}

#[test]
fn the_plugin_profile_turns_off_every_plugin_but_the_kept_ones() {
    let home = std::env::temp_dir().join(format!("wg-plugins-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".claude/plugins")).unwrap();
    std::fs::write(
        home.join(".claude/plugins/installed_plugins.json"),
        r#"{"version":2,"plugins":{"ecc@ecc":[],"ponytail@ponytail":[]}}"#,
    )
    .unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"enabledPlugins":{"haruspex@haruspex":true,"cc-plugin-x@builtin":true}}"#,
    )
    .unwrap();
    let keep = vec!["ecc".to_string(), "haruspex".to_string()];
    let got = plugin_profile(&home, &keep);
    assert_eq!(
        got,
        [
            ("cc-plugin-x@builtin".to_string(), false),
            ("ecc@ecc".to_string(), true),
            ("haruspex@haruspex".to_string(), true),
            ("ponytail@ponytail".to_string(), false),
        ]
    );
    assert!(plugin_profile(Path::new("/nonexistent"), &keep).is_empty());
    assert_eq!(shell_quote("a'b"), r"'a'\''b'");
}
