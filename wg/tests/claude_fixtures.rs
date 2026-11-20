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
    /// A paired `<name>.init.json` excerpt of the stream-json init event exists.
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
    if let Some(source) = c.api_key_source {
        let init = load(&format!("{n}.init.json"));
        assert_eq!(init["subtype"], "init", "{n}: init event");
        assert_eq!(init["apiKeySource"], source, "{n}: apiKeySource");
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

#[test]
fn project_mcp_servers_start_only_without_both_isolation_flags() {
    // §5.1: the repo's .mcp.json server appears (and was spawned) only when neither
    // --setting-sources user nor --strict-mcp-config is given.
    let project = |n: &str| {
        load(&format!("project_config_{n}.init.json"))["project_mcp_servers"]
            .as_array()
            .expect("project_mcp_servers")
            .len()
    };
    assert_eq!(project("no_flags"), 1);
    assert_eq!(project("both_flags"), 0);
    assert_eq!(project("setting_sources_only"), 0);
    assert_eq!(project("strict_mcp_only"), 0);
    let both = load("project_config_both_flags.init.json");
    assert_eq!(both["mcp_server_count_by_source"], serde_json::json!({}));
}

#[test]
fn a_settings_profile_turns_plugins_off_per_call() {
    let names = |f: &str| -> BTreeSet<String> {
        load(f)["plugins"]
            .as_array()
            .expect("plugins")
            .iter()
            .map(|p| p.as_str().expect("plugin name").to_owned())
            .collect()
    };
    let on = names("plugins_default.init.json");
    let off = names("plugins_profile_off.init.json");
    for p in ["ponytail", "superpowers", "i-have-adhd", "workgraph"] {
        assert!(on.contains(p) && !off.contains(p), "{p} not turned off");
    }
    for p in ["ecc", "haruspex", "mattpocock-skills"] {
        assert!(off.contains(p), "{p} must stay on");
    }
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

#[test]
fn no_fixture_carries_a_key_or_an_email() {
    for entry in std::fs::read_dir(dir()).expect("fixtures dir") {
        let path = entry.expect("dir entry").path();
        let text = std::fs::read_to_string(&path).expect("fixture text");
        let name = path.display();
        assert!(!text.contains("sk-ant-api"), "{name}: key-shaped string");
        assert!(!has_email(&text), "{name}: email");
    }
}
