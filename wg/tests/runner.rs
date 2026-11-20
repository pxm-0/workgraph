//! The runner against scripted `claude` envelopes (design §15.3 `runner.rs`): provider fallback on
//! a rate limit, the rerun rule, a missing cost, gateway outcomes and key handling.

mod support;

use serde_json::{json, Value};
use std::sync::Arc;
use support::*;
use wg::ports::RawOutput;

const PROVIDERS: &str = r#"
[providers.subscription]
kind = "subscription"
models = { haiku = "haiku", sonnet = "sonnet", opus = "opus" }

[providers.anthropic-api]
kind = "anthropic-api"
keychain_service = "workgraph-anthropic"
models = { haiku = "claude-haiku-5-5", sonnet = "claude-sonnet-5-5", opus = "claude-opus-5-5" }
"#;

fn limited() -> RawOutput {
    let v = json!({ "type": "result", "subtype": "success", "is_error": true, "api_error_status": 429,
                    "result": "usage limit", "total_cost_usd": 0, "modelUsage": {} });
    RawOutput {
        exit: Some(1),
        timed_out: false,
        stdout: v.to_string(),
        stderr: String::new(),
    }
}

fn ledger(o: &Out) -> Vec<Value> {
    let text = std::fs::read_to_string(o.home.join(".workgraph/cost.jsonl")).unwrap();
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn model_of(s: &wg::ports::Spawn) -> String {
    let i = s.argv.iter().position(|a| a == "--model").unwrap();
    s.argv[i + 1].clone()
}

#[test]
fn a_subscription_limit_falls_back_to_the_api_for_the_rest_of_the_run() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        config_extra: PROVIDERS.into(),
        raw: Arc::new(|s| (model_of(s) == "opus" && s.label == "read").then(limited)),
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "ready_for_review");
    let reads: Vec<&str> = o
        .proc_calls
        .iter()
        .filter(|c| c.label == "read")
        .map(|c| c.model.as_str())
        .collect();
    assert_eq!(
        reads,
        ["opus", "claude-opus-5-5"],
        "the same call reruns once on the next provider"
    );
    assert_eq!(
        o.model("challenge"),
        "claude-sonnet-5-5",
        "the switch stays for the run"
    );
    let l = ledger(&o);
    assert_eq!(
        (l[0]["provider"].as_str(), l[0]["outcome"].as_str()),
        (Some("subscription"), Some("RateLimited"))
    );
    assert_eq!(l[1]["provider"], "anthropic-api");
    assert_eq!(l[1]["switched_after"], "rate limited on subscription");
    assert!(
        l.iter()
            .all(|x| x["provider"].is_string() && x["model_id"].is_string()),
        "every call names provider and model"
    );
    let settings = std::fs::read_dir(o.run_dir.join("guard"))
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.to_string_lossy()
                .ends_with(".settings.json")
                .then(|| std::fs::read_to_string(p).unwrap())
        });
    let helpers = settings.filter(|t| t.contains("apiKeyHelper")).count();
    assert!(helpers > 0, "API calls carry the helper");
    for c in &o.proc_calls {
        assert!(
            c.env
                .iter()
                .all(|(k, _)| !k.contains("KEY") && !k.contains("TOKEN")),
            "{:?}",
            c.env
        );
        let base = c.env.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL");
        assert!(!base, "no base URL on first-party profiles");
    }
}

#[test]
fn with_no_usable_provider_left_the_task_parks() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        config_extra: PROVIDERS.into(),
        raw: Arc::new(|s| (s.label == "read").then(limited)),
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "parked");
    assert!(o.result["note"].as_str().unwrap().contains("rate limited"));
    assert_eq!(o.proc_calls.len(), 2);
}

#[test]
fn a_limited_call_that_may_have_edited_is_not_rerun() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        config_extra: PROVIDERS.into(),
        raw: Arc::new(|s| {
            if s.label != "work:a" || model_of(s) != "haiku" {
                return None;
            }
            let manifest = &s
                .env
                .iter()
                .find(|(k, _)| k == "WG_GUARD_MANIFEST")
                .unwrap()
                .1;
            let m: Value =
                serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
            let line =
                json!({ "label": "work:a", "decision": "allow", "effect": true, "tool": "Edit" });
            std::fs::write(m["log"].as_str().unwrap(), format!("{line}\n")).unwrap();
            Some(limited())
        }),
        ..Opts::default()
    });
    assert_eq!(
        o.proc_calls.iter().filter(|c| c.label == "work:a").count(),
        1,
        "no rerun after an edit"
    );
    assert_eq!(o.result["stuck"][0]["why"], "worker died");
    assert_eq!(
        o.model("learn"),
        "claude-sonnet-5-5",
        "later calls use the next provider"
    );
}

#[test]
fn a_missing_cost_is_recorded_as_unknown_never_zero() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        raw: Arc::new(|s| {
            (s.label == "challenge").then(|| {
                let v = json!({ "type": "result", "subtype": "success", "is_error": false,
                                "structured_output": { "added": [], "assign": [], "escalations": [] } });
                RawOutput { exit: Some(0), timed_out: false, stdout: v.to_string(), stderr: String::new() }
            })
        }),
        ..Opts::default()
    });
    let l = ledger(&o);
    let ch = l.iter().find(|x| x["label"] == "challenge").unwrap();
    assert_eq!(ch["cost_micros"], "unknown");
    assert_eq!(o.result["callsWithUnknownCost"], 1);
    assert!(o.journal.iter().any(|e| e["warning"]
        .as_str()
        .is_some_and(|w| w.contains("cost recorded as unknown"))));
    assert!(o.result["costUsd"].as_f64().unwrap() > 0.0);
}

#[test]
fn gateway_outcomes_are_ledgered_as_unknown_and_never_move_tier_stats() {
    let gateway = r#"
[providers.litellm]
kind = "gateway"
base_url = "http://127.0.0.1:4000"
models = { haiku = "local-small", sonnet = "local-mid", opus = "local-big" }

[auth]
default = "litellm"
fallback = []
"#;
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        config_extra: gateway.into(),
        ..Opts::default()
    });
    assert_eq!(o.result["status"], "ready_for_review");
    assert_eq!(o.model("work:a"), "local-small");
    assert!(o
        .call("work:a")
        .env
        .contains(&("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:4000".into())));
    assert!(ledger(&o).iter().all(|x| x["cost_micros"] == "unknown"));
    assert_eq!(
        o.saved.unwrap()["models"],
        json!({}),
        "no tier evidence from a gateway"
    );
}

#[test]
fn every_call_runs_with_the_cleared_env_and_the_guard_hook() {
    let o = run(Opts {
        units: vec![unit("a", json!({}))],
        ..Opts::default()
    });
    for c in &o.proc_calls {
        let names: Vec<&str> = c.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "HOME",
                "USER",
                "LANG",
                "TERM",
                "PATH",
                "TMPDIR",
                "WG_RUN",
                "WG_GUARD_MANIFEST",
                "WG_AGENT",
                "ARGUS_SSH_HOST"
            ]
        );
        for f in [
            "--permission-mode",
            "--setting-sources",
            "--strict-mcp-config",
            "--settings",
            "--no-session-persistence",
        ] {
            assert!(c.argv.iter().any(|a| a == f), "{} lacks {f}", c.label);
        }
        assert!(
            !c.argv.iter().any(|a| a == "Bash" || a == "Skill"),
            "never bare Bash or Skill"
        );
    }
    let work = o.call("work:a");
    assert!(work
        .argv
        .iter()
        .any(|a| a.starts_with("Edit(//") && a.ends_with("/src/a.js)")));
}
