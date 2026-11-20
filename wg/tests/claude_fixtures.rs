//! Phase 0 contract spike (ADR-001): every recorded `claude -p` envelope in
//! `tests/fixtures/claude/` parses with serde_json and carries the fields design §5.3 relies on.
//! Every fixture file must appear in `CASES` and in the fixtures README, so none goes untested.
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const HAIKU: &str = "claude-haiku-5-5";
const SONNET: &str = "claude-sonnet-5-5";

/// What a result envelope must hold.
struct Case {
    name: &'static str,
    subtype: &'static str,
    is_error: bool,
    /// `structured_output` must be a JSON object (a `--json-schema` success).
    structured: bool,
    /// Exact `modelUsage` keys; empty when the call never reached a model.
    models: &'static [&'static str],
    /// `permission_denials` length, when the case is about permissions.
    denials: Option<usize>,
    /// A paired `<name>.init.json` (the whole stream-json init event, redacted) exists.
    init: bool,
    /// `apiKeySource` in that excerpt, when the case is about the billing path.
    api_key_source: Option<&'static str>,
}

const fn case(name: &'static str, subtype: &'static str, is_error: bool) -> Case {
    Case {
        name,
        subtype,
        is_error,
        structured: false,
        models: &[HAIKU],
        denials: None,
        init: false,
        api_key_source: None,
    }
}

const fn ok(name: &'static str) -> Case {
    case(name, "success", false)
}

const fn structured(name: &'static str) -> Case {
    Case {
        structured: true,
        ..ok(name)
    }
}

const fn denied(name: &'static str, n: usize) -> Case {
    Case {
        denials: Some(n),
        ..ok(name)
    }
}

const fn no_model(name: &'static str) -> Case {
    Case {
        models: &[],
        ..case(name, "success", true)
    }
}

const fn with_init(c: Case) -> Case {
    Case { init: true, ..c }
}

const fn with_source(c: Case, source: &'static str) -> Case {
    Case {
        api_key_source: Some(source),
        ..with_init(c)
    }
}

const CASES: &[Case] = &[
    // §16 contract envelopes
    structured("success_structured"),
    structured("schema_max_turns_1"),
    no_model("is_error_bad_model"),
    case("budget_guard", "error_max_budget_usd", true),
    case("max_turns", "error_max_turns", true),
    Case {
        models: &[SONNET, HAIKU],
        ..ok("multi_model_usage")
    },
    // permissions (§5.1, §5.2)
    denied("perm_dontask", 3),
    denied("perm_edit_scope", 4),
    ok("perm_edit_anchored_dot"),
    ok("perm_edit_anchored_abs"),
    denied("perm_bash_chain", 2),
    denied("guard_metachar", 1),
    denied("hook_exit2", 1),
    denied("hook_abort", 1),
    denied("hook_ask_dontask", 1),
    denied("hook_ask_default", 1),
    denied("skill_scope", 0),
    denied("skill_disallow", 1),
    denied("skill_guard_hook", 1),
    denied("skill_frontmatter_widen", 1),
    // project config isolation (§5.1)
    with_init(denied("project_config_both_flags", 0)),
    with_init(denied("project_config_no_flags", 0)),
    with_init(denied("project_config_setting_sources_only", 0)),
    with_init(denied("project_config_strict_mcp_only", 0)),
    with_init(ok("plugins_default")),
    with_init(ok("plugins_profile_off")),
    // billing paths (D5) and gateway profiles (D6)
    with_source(ok("subscription_no_helper"), "none"),
    with_source(no_model("api_key_helper_invalid"), "apiKeyHelper"),
    with_source(no_model("gateway_loopback_dry_run"), "apiKeyHelper"),
    no_model("gateway_loopback_provider_model"),
    // per-stage cost fixtures, two toy tasks
    structured("stage_t1_read"),
    structured("stage_t1_challenge"),
    structured("stage_t1_work"),
    structured("stage_t1_fix"),
    structured("stage_t1_review"),
    structured("stage_t1_learn"),
    structured("stage_t2_read"),
    structured("stage_t2_challenge"),
    structured("stage_t2_work"),
    structured("stage_t2_fix"),
    structured("stage_t2_review"),
    structured("stage_t2_learn"),
];

/// Fixture files that are not result envelopes.
const OTHER_FILES: &[&str] = &["rate_limit_event.json"];

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude")
}

fn load(file: &str) -> Value {
    let path = dir().join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// The `modelUsage` key with the most output tokens (§5.3 step 4).
fn dominant_model(usage: &serde_json::Map<String, Value>) -> Option<&str> {
    usage
        .iter()
        .max_by_key(|(_, v)| v["outputTokens"].as_u64().unwrap_or(0))
        .map(|(k, _)| k.as_str())
}

fn check(c: &Case) {
    let env = load(&format!("{}.json", c.name));
    let n = c.name;
    assert_eq!(env["type"], "result", "{n}: type");
    assert_eq!(env["subtype"], c.subtype, "{n}: subtype");
    assert_eq!(env["is_error"], c.is_error, "{n}: is_error");
    let cost = env["total_cost_usd"].as_f64();
    assert!(cost.is_some_and(|x| x >= 0.0), "{n}: total_cost_usd");
    assert!(env["session_id"].is_string(), "{n}: session_id");
    assert!(env["num_turns"].is_u64(), "{n}: num_turns");

    let usage = env["modelUsage"].as_object().expect("modelUsage object");
    let keys: BTreeSet<&str> = usage.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        c.models.iter().copied().collect(),
        "{n}: modelUsage keys"
    );
    if let Some(first) = c.models.first() {
        assert_eq!(dominant_model(usage), Some(*first), "{n}: dominant model");
        assert!(
            usage.values().all(|m| m["costUSD"].is_number()),
            "{n}: costUSD"
        );
    } else {
        assert_eq!(cost, Some(0.0), "{n}: a call that reached no model costs 0");
    }

    let so = &env["structured_output"];
    if c.structured {
        assert!(so.is_object(), "{n}: structured_output must be an object");
        // §5.3: the free-text result is never parsed for values, but it mirrors the object.
        let mirrored: Value = serde_json::from_str(env["result"].as_str().unwrap()).unwrap();
        assert_eq!(&mirrored, so, "{n}: result mirrors structured_output");
    } else {
        assert!(so.is_null(), "{n}: no structured_output without a schema");
    }

    if let Some(d) = c.denials {
        let got = env["permission_denials"]
            .as_array()
            .expect("permission_denials");
        assert_eq!(got.len(), d, "{n}: permission_denials");
    }
    if c.init {
        let init = load(&format!("{n}.init.json"));
        assert_eq!(init["subtype"], "init", "{n}: init event");
        assert!(init["plugins"].is_array(), "{n}: init plugins");
        assert!(init["mcp_servers"].is_array(), "{n}: init mcp_servers");
        if let Some(source) = c.api_key_source {
            assert_eq!(init["apiKeySource"], source, "{n}: apiKeySource");
        }
    }
}

#[test]
fn every_fixture_parses_and_holds_the_contract_fields() {
    CASES.iter().for_each(check);
}

#[test]
fn every_fixture_file_is_covered_and_documented() {
    let readme = std::fs::read_to_string(dir().join("README.md")).expect("fixtures README");
    let covered: BTreeSet<String> = CASES
        .iter()
        .flat_map(|c| {
            let init = c.init.then(|| format!("{}.init.json", c.name));
            std::iter::once(format!("{}.json", c.name)).chain(init)
        })
        .chain(OTHER_FILES.iter().map(|f| f.to_string()))
        .collect();
    let on_disk: BTreeSet<String> = std::fs::read_dir(dir())
        .expect("fixtures dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter(|f| f.ends_with(".json"))
        .collect();
    assert_eq!(on_disk, covered, "fixture files and CASES differ");
    for c in CASES {
        assert!(
            readme.contains(&format!("`{}`", c.name)),
            "README lacks {}",
            c.name
        );
    }
    for f in OTHER_FILES {
        assert!(readme.contains(&format!("`{f}`")), "README lacks {f}");
    }
}

#[test]
fn failure_subtypes_are_distinct_from_is_error_alone() {
    // §5.3: an API error ends with subtype "success" and is_error true, so the parser must check
    // both fields; subtype alone would call it a success.
    let env = load("is_error_bad_model.json");
    assert_eq!(env["subtype"], "success");
    assert_eq!(env["is_error"], true);
    assert_eq!(env["api_error_status"], 404);
}

/// `mcp_servers` of a case's init event whose `source` is `source`.
fn servers_from(case: &str, source: &str) -> usize {
    load(&format!("{case}.init.json"))["mcp_servers"]
        .as_array()
        .expect("mcp_servers")
        .iter()
        .filter(|s| s["source"] == source)
        .count()
}

/// True if the spike left `<spike>/markers/<name>` (a hook or MCP server touched it).
fn marker(name: &str) -> bool {
    dir().join("spike/markers").join(name).exists()
}

#[test]
fn project_config_runs_only_without_both_isolation_flags() {
    // §5.1: the repo's .mcp.json server appears (and was spawned) only when neither
    // --setting-sources user nor --strict-mcp-config is given.
    let project = |v: &str| servers_from(&format!("project_config_{v}"), "project");
    assert_eq!(project("no_flags"), 1);
    assert_eq!(project("both_flags"), 0);
    assert_eq!(project("setting_sources_only"), 0);
    assert_eq!(project("strict_mcp_only"), 0);
    let both = load("project_config_both_flags.init.json");
    assert_eq!(
        both["mcp_servers"],
        serde_json::json!([]),
        "no server at all"
    );
    // Without --strict-mcp-config the owner's plugin and claude.ai servers load too.
    assert!(servers_from("project_config_setting_sources_only", "claudeai") > 0);
    assert_eq!(
        servers_from("project_config_strict_mcp_only", "claudeai"),
        0
    );
    // The project PreToolUse hook and MCP server touch a marker per variant
    // (spike/project_config/pc_<variant>.*): the hook fired only without --setting-sources user.
    assert!(marker("project_hook_none") && marker("project_hook_strict"));
    assert!(!marker("project_hook_both") && !marker("project_hook_ss"));
    assert!(marker("project_mcp_none"));
    for v in ["both", "ss", "strict"] {
        assert!(!marker(&format!("project_mcp_{v}")), "project_mcp_{v}");
    }
}

#[test]
fn a_settings_profile_turns_plugins_off_per_call() {
    // enabledPlugins keys are `name@marketplace`: the init event's `plugins[].source`.
    let sources = |f: &str| -> BTreeSet<(String, String)> {
        load(f)["plugins"]
            .as_array()
            .expect("plugins")
            .iter()
            .map(|p| {
                let name = p["name"].as_str().expect("plugin name").to_owned();
                let source = p["source"].as_str().expect("plugin source").to_owned();
                assert!(source.starts_with(&format!("{name}@")), "{source}");
                (name, source)
            })
            .collect()
    };
    let names = |s: &BTreeSet<(String, String)>| -> BTreeSet<String> {
        s.iter().map(|(n, _)| n.clone()).collect()
    };
    let on = names(&sources("plugins_default.init.json"));
    let off = names(&sources("plugins_profile_off.init.json"));
    for p in ["ponytail", "superpowers", "i-have-adhd", "workgraph"] {
        assert!(on.contains(p) && !off.contains(p), "{p} not turned off");
    }
    for p in ["ecc", "haruspex", "mattpocock-skills"] {
        assert!(off.contains(p), "{p} must stay on");
    }
}

#[test]
fn rate_limit_event_carries_the_subscription_window_status() {
    // §5 candidate usage-limit signal (not yet seen in a limited state): stream-json emits it.
    let e = load("rate_limit_event.json");
    assert_eq!(e["type"], "rate_limit_event");
    let info = &e["rate_limit_info"];
    assert_eq!(info["status"], "allowed");
    assert!(info["rateLimitType"].is_string() && info["resetsAt"].is_u64());
    for window in ["five_hour", "seven_day"] {
        let w = &info["unifiedWindows"][window];
        assert!(w["utilization"].is_f64(), "{window}: utilization");
        assert!(w["status"].is_string(), "{window}: status");
    }
}

#[test]
fn gateway_dry_run_resolves_aliases_client_side() {
    // The init event shows `--model sonnet` resolved before the call; the stub log (kept for the
    // provider-model run only) shows an arbitrary id reaching the gateway unchanged.
    let init = load("gateway_loopback_dry_run.init.json");
    assert_eq!(init["model"], SONNET);
    let log = std::fs::read_to_string(dir().join("spike/markers/gateway.log")).expect("stub log");
    let posts: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).expect("stub log line"))
        .filter(|r: &Value| r["method"] == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    assert!(posts
        .iter()
        .all(|r| r["model"] == "openai/gpt-5.1" && r["auth_header"] == "x-api-key"));
}

#[test]
fn num_turns_can_exceed_max_turns() {
    // ADR §1: ledger num_turns as reported; never derive limits from max_turns + 1.
    // (fixture, --max-turns): see spike/stages.sh and spike/argv.txt.
    for (f, max) in [
        ("schema_max_turns_1", 1),
        ("stage_t1_challenge", 8),
        ("stage_t2_challenge", 8),
        ("stage_t2_read", 8),
        ("stage_t1_learn", 3),
    ] {
        let env = load(&format!("{f}.json"));
        assert_eq!(env["subtype"], "success", "{f}");
        assert!(env["num_turns"].as_u64().unwrap() > max, "{f}");
    }
    assert_eq!(load("stage_t1_challenge.json")["num_turns"], 10);
}

/// True if `text` holds something shaped like `local@domain.tld`.
fn has_email(text: &str) -> bool {
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    text.match_indices('@').any(|(i, _)| {
        let before = text[..i].chars().next_back().is_some_and(local);
        let domain: String = text[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect();
        let tld = domain.rsplit('.').next().unwrap_or("");
        before
            && domain.contains('.')
            && tld.len() >= 2
            && tld.chars().all(|c| c.is_ascii_alphabetic())
    })
}

#[test]
fn email_detector_works() {
    assert!(has_email("mail me at a.b@example.com now"));
    assert!(!has_email("Bash(git status:*) and @types/node"));
}

/// Every file under `root`, recursively.
fn files_under(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .expect("fixtures dir")
        .flat_map(|e| {
            let path = e.expect("dir entry").path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

#[test]
fn no_fixture_carries_a_key_an_email_or_a_home_path() {
    let files = files_under(&dir());
    assert!(
        files.iter().any(|p| p.ends_with("spike/run.sh")),
        "walks spike/"
    );
    for path in files {
        let text = std::fs::read_to_string(&path).expect("fixture text");
        let name = path.display();
        assert!(!text.contains("sk-ant-api"), "{name}: key-shaped string");
        assert!(!has_email(&text), "{name}: email");
        assert!(!text.contains("/Users/"), "{name}: unredacted home path");
    }
}
