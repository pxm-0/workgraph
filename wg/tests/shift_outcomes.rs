//! Every §7.3 step 8 outcome and the preflight branches of `wg shift run` (design §15.4), against a
//! scripted claude, a fake gh and the real Haruspex CLI: repos skipped as RepoNotCompliant or for a
//! failing intake, an install failure, a failing check through the factory compose prefix, a
//! kernel error, a publish refused for a file outside the plan, a branch switch, and an incident
//! that stops the shift; then the per-stage provider probes, a missing keychain item, and a publish
//! that a STOP mid-publish or an existing PR must not lose or repeat.

mod shift_support;

use serde_json::json;
use shift_support::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn why(r: &serde_json::Value, key: &str, repo: &str) -> String {
    r[key]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["repo"] == repo)
        .and_then(|e| e["why"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn every_outcome_gets_its_label_and_comment_and_an_incident_stops_the_shift() {
    let Some(cli) = cli() else { return };
    let repos = [
        "o/a", "o/b", "o/i", "o/c", "o/d", "o/e", "o/f", "o/g", "o/h",
    ];
    let f = fx(
        "outcomes",
        &cli,
        &repos,
        "max_consecutive_failures = 10\nmax_tasks = 20",
    );
    f.push("o/a", "git rm -rq .devcontainer");
    f.push("o/b", "echo '{}' > .mcp.json && git add .mcp.json");
    f.set_repo("o/c", "install", "\"false\"");
    // A fake `docker` logs each command, drops `compose -p <name> -f <override>` and runs the rest.
    let docker = f.base.join("bin/docker");
    let log = f.base.join("docker.log");
    let over = f.base.join("o_d.factory.yml");
    fs::create_dir_all(f.base.join("bin")).unwrap();
    fs::write(&over, "services: {}\n").unwrap();
    fs::write(
        &docker,
        format!(
            "#!/bin/sh\necho \"$*\" >> {}\nshift 5\nexec \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let prefix = format!("docker compose -p wg-o-d -f {}", over.display());
    f.set_repo("o/d", "factory_compose", &format!("\"{prefix}\""));
    // Another live run holds o/e's memory lock: a kernel error after the claim.
    fs::create_dir_all(f.home.join(".workgraph/memory")).unwrap();
    fs::write(
        f.home.join(".workgraph/memory/o__e.lock"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    for (r, body) in [
        ("o/a", "add(2, 3) must be 5."),
        ("o/b", "add(2, 3) must be 5."),
        ("o/i", "add(2, 3) must be 5."),
        ("o/c", "add(2, 3) must be 5."),
        ("o/d", "FAIL-CHECK: add(2, 3) must be 6."),
        ("o/e", "add(2, 3) must be 5."),
        ("o/f", "WORKFLOW: add(2, 3) must be 5."),
        ("o/g", "WRONG-BRANCH: add(2, 3) must be 5."),
        ("o/h", "INCIDENT: add(2, 3) must be 5."),
    ] {
        f.issue(r, 1, "Fix add", body, "pxm-0", "pxm-0");
    }
    f.issue("o/h", 2, "Later", "add(2, 3) must be 5.", "pxm-0", "pxm-0");
    // The first issue list (o/i's, since o/a and o/b are skipped first) fails every attempt.
    let mut st = f.state();
    st["fail"] = json!({ "issue list": 3 });
    f.set_state(&st);

    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    let r = f.report(&id);
    assert!(
        why(&r, "skippedRepos", "o/a")
            .contains("RepoNotCompliant: origin/main has no .devcontainer"),
        "{r}"
    );
    assert!(
        why(&r, "skippedRepos", "o/b")
            .contains("RepoNotCompliant: project agent config .mcp.json on origin/main"),
        "{r}"
    );
    assert!(why(&r, "skippedRepos", "o/i").starts_with("intake: "));
    for skipped in ["o/a", "o/b", "o/i"] {
        assert_eq!(f.labels(skipped, 1), ["wg:queued"], "{skipped}");
        assert!(
            f.comments(skipped, 1).is_empty(),
            "{skipped}: never claimed"
        );
    }

    let comment = |repo: &str| f.comments(repo, 1).last().cloned().unwrap_or_default();
    assert_eq!(f.labels("o/c", 1), ["wg:stuck"]);
    assert!(
        comment("o/c").contains("install failed (exit 1)"),
        "{}",
        comment("o/c")
    );

    assert_eq!(f.labels("o/d", 1), ["wg:stuck"]);
    assert!(
        comment("o/d").contains("add(2, 3) is not 6"),
        "the comment carries the check's tail: {}",
        comment("o/d")
    );
    let wrapped = fs::read_to_string(&log).unwrap();
    let ran = |cmd: &str| format!("{} {cmd}", prefix.trim_start_matches("docker "));
    assert!(
        wrapped.lines().any(|l| l == ran("true"))
            && wrapped.lines().any(|l| l == ran("python3 check.py")),
        "install and checks run through the factory compose prefix: {wrapped}"
    );

    assert_eq!(f.labels("o/e", 1), ["wg:stuck"]);
    assert!(
        comment("o/e").contains("another run holds"),
        "{}",
        comment("o/e")
    );
    assert_eq!(
        f.record("o/e", 1)["status"],
        "stuck",
        "recover never requeues it"
    );

    assert_eq!(f.labels("o/f", 1), ["wg:stuck"]);
    assert!(
        comment("o/f").contains("CI configuration"),
        "{}",
        comment("o/f")
    );
    assert_eq!(
        sh(&f.origin("o/f"), "git branch --list 'wg/*'"),
        "",
        "nothing was pushed"
    );

    assert_eq!(f.labels("o/g", 1), ["wg:stuck"]);
    assert!(
        comment("o/g").contains("not the task branch"),
        "{}",
        comment("o/g")
    );

    assert_eq!(f.labels("o/h", 1), ["wg:incident"]);
    assert!(
        comment("o/h").contains("class: guard") && !comment("o/h").contains("wg shift run"),
        "{}",
        comment("o/h")
    );
    assert_eq!(f.labels("o/h", 2), ["wg:queued"], "the shift stopped first");
    assert_eq!(out["stopped"], json!(["an incident"]));
    assert_eq!(out["needsYou"]["incidents"], 1);
    assert_eq!(r["stuck"].as_array().unwrap().len(), 5, "{r}");
    assert!(f.prs().is_empty());
}

const MODELS: &str = "models = { haiku = \"m-haiku\", sonnet = \"m-sonnet\", opus = \"m-opus\" }";

#[test]
fn preflight_probes_each_stage_provider_and_stops_on_a_missing_keychain_item() {
    let Some(cli) = cli() else { return };
    let f = fx("preflight", &cli, &["o/r"], "");
    f.append_toml(&format!(
        "[providers.subscription]\nkind = \"subscription\"\n{MODELS}\n\n\
         [providers.gw]\nkind = \"gateway\"\nbase_url = \"http://127.0.0.1:9\"\n{MODELS}\n\n\
         [auth]\nfallback = [\"subscription\"]\n\n[auth.stages]\nlearn = \"gw\"\n"
    ));
    let out = f.shift();
    assert_eq!(out["stopped"], json!([]), "{out}");
    let probed: Vec<String> = f
        .ledger()
        .iter()
        .filter(|l| l["stage"] == "doctor")
        .map(|l| l["provider"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(probed, ["subscription", "gw"]);

    let nokey = f.base.join("security.sh");
    fs::write(&nokey, "#!/bin/sh\nexit 44\n").unwrap();
    fs::set_permissions(&nokey, fs::Permissions::from_mode(0o755)).unwrap();
    let text = fs::read_to_string(f.toml())
        .unwrap()
        .replacen(
            "[tools]\n",
            &format!("[tools]\nsecurity = \"{}\"\n", nokey.display()),
            1,
        )
        .replace(
            "fallback = [\"subscription\"]",
            "fallback = [\"subscription\", \"api\"]",
        );
    fs::write(f.toml(), text).unwrap();
    f.append_toml(&format!(
        "[providers.api]\nkind = \"anthropic-api\"\nkeychain_service = \"wg-test-missing\"\n{MODELS}\n"
    ));
    f.issue(
        "o/r",
        1,
        "Fix add",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    let out = f.shift();
    assert_eq!(
        out["stopped"],
        json!(["preflight: provider api: keychain item wg-test-missing is missing"])
    );
    assert_eq!(f.labels("o/r", 1), ["wg:queued"]);
}

#[test]
fn a_stop_during_publish_keeps_the_pr_and_an_existing_pr_is_never_opened_twice() {
    let Some(cli) = cli() else { return };
    let f = fx("publishstop", &cli, &["o/r"], "");
    f.issue(
        "o/r",
        1,
        "Fix add",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    f.issue(
        "o/r",
        2,
        "Fix add again",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    let stop = f.home.join(".workgraph/STOP");
    let mut st = f.state();
    st["touch"] = json!({ "pr create": stop });
    f.set_state(&st);

    let out = f.shift();
    assert_eq!(out["stopped"], json!(["the STOP file exists"]), "{out}");
    assert_eq!(f.labels("o/r", 1), ["wg:pr-open"]);
    assert_eq!(f.record("o/r", 1)["pr"], 100);
    assert_eq!(out["needsYou"]["approvals"], 1);
    assert_eq!(f.labels("o/r", 2), ["wg:queued"]);

    // Issue 2 already has a PR for its branch (an earlier run pushed and opened it).
    fs::remove_file(&stop).unwrap();
    let mut st = f.state();
    st["touch"] = json!({});
    st["prs"].as_array_mut().unwrap().push(json!({
        "repo": "o/r", "number": 200, "url": "https://github.com/o/r/pull/200", "state": "OPEN",
        "headRefName": "wg/2-fix-add-again", "draft": true, "labels": [] }));
    f.set_state(&st);
    f.shift();
    assert_eq!(f.labels("o/r", 2), ["wg:pr-open"]);
    assert_eq!(f.record("o/r", 2)["pr"], 200);
    assert_eq!(f.prs().len(), 2);
    let creates = f.state()["log"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a[0] == "pr" && a[1] == "create")
        .count();
    assert_eq!(creates, 1, "the existing PR was returned, not opened again");
}
