//! The self lane in a shift (design §7.3 step 11, §12.1, §15.4), end to end with the fake claude,
//! the fake gh, the fake orb sandbox and the real Haruspex CLI: an owner-queued `wg:self` issue
//! becomes a ready `wg:self` PR on a `wg-self/*` branch whose head the self-ledger records, the
//! gate decides it in shadow mode; a stranger's label is ignored; a plan naming a protected path
//! ends before any edit; and a `factory` proposal is filed as `wg:self-proposed`, never worked.

mod shift_support;

use serde_json::{json, Value};
use shift_support::*;

const SELF: &str = "pxm-0/workgraph";

fn self_issue(f: &Fx, n: u64, body: &str, labeller: &str) {
    f.issue(SELF, n, "Document add", body, "pxm-0", "pxm-0");
    let mut s = f.state();
    s["issues"][SELF][n.to_string()]["labels"] = json!([{ "name": "wg:self" }]);
    s["issues"][SELF][n.to_string()]["events"] =
        json!([{ "label": "wg:self", "actor": labeller, "at": "2026-11-01T11:00:00Z" }]);
    f.set_state(&s);
}

fn self_fx(name: &str, cli: &str, repos: &[&str]) -> Fx {
    let f = fx(name, cli, repos, "");
    f.set_repo(SELF, "pipelines", "[\"self\"]");
    f.set_repo(SELF, "always_tainted", "true");
    f.with_sandbox(false);
    f.append_toml("[self_improve]\nenabled = true\n");
    let protected =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/protected.toml");
    f.push(
        SELF,
        &format!(
            "mkdir -p config && cp {} config/ && git add config",
            protected.display()
        ),
    );
    let mut s = f.state();
    s["check_runs"] = json!(wg::selfimprove::protected::compiled()
        .required_checks
        .iter()
        .map(|(wf, job)| json!({ "name": job, "workflow": wf }))
        .collect::<Vec<_>>());
    f.set_state(&s);
    f
}

fn self_ledger(f: &Fx) -> Vec<Value> {
    f.journal(&f.home.join(".workgraph/self-ledger.jsonl"))
}

#[test]
fn an_owner_queued_self_issue_becomes_a_ready_self_pr_the_gate_shadows() {
    let Some(cli) = cli() else { return };
    let f = self_fx("selflane", &cli, &[SELF]);
    self_issue(&f, 5, "SELF-DOCS Document what add does.", "pxm-0");
    self_issue(&f, 6, "SELF-DOCS A stranger queued this.", "stranger");
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    let prs = f.prs();
    let shift_journal = f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(&id)
            .join("journal.jsonl"),
    );
    assert_eq!(prs.len(), 1, "{shift_journal:?}");
    let pr = &prs[0];
    assert_eq!(pr["headRefName"], "wg-self/5-document-add");
    assert_eq!(
        (pr["draft"].as_bool(), pr["base"].as_str()),
        (Some(false), Some("main"))
    );
    assert!(
        pr["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "wg:self"),
        "{pr}"
    );
    assert!(
        pr["labels"].to_string().contains("wg:self-shadow"),
        "the gate decided it: {pr}"
    );
    let labels = f.labels(SELF, 5);
    assert!(
        labels.contains(&"wg:self".to_string()) && labels.contains(&"wg:pr-open".to_string()),
        "{labels:?}"
    );
    assert!(
        f.labels(SELF, 6).iter().all(|l| l == "wg:self"),
        "a stranger's label is never work"
    );

    let origin = f.origin(SELF);
    let head = sh(&origin, "git rev-parse wg-self/5-document-add");
    let files = sh(
        &origin,
        "git show --name-only --format= wg-self/5-document-add",
    );
    assert_eq!(
        files, "docs/calc.md",
        "Haruspex records never ship in a self PR"
    );
    let msg = sh(&origin, "git log -1 --format=%B wg-self/5-document-add");
    let ledger = self_ledger(&f);
    let authored = ledger.iter().find(|e| e["kind"] == "authored").unwrap();
    assert_eq!(authored["head"], head.as_str());
    assert!(
        msg.contains(&format!(
            "Workgraph-Run: {}",
            authored["run"].as_str().unwrap()
        )),
        "{msg}"
    );
    let decision = ledger.iter().find(|e| e["kind"] == "decision").unwrap();
    assert_eq!(
        (decision["outcome"].as_str(), decision["shadow"].as_bool()),
        (Some("shadow"), Some(true))
    );
    let journal = f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(&id)
            .join("journal.jsonl"),
    );
    assert!(
        journal
            .iter()
            .any(|e| e["kind"] == "self_gate" && e["outcome"] == "shadow"),
        "{journal:?}"
    );
    let run = f.record(SELF, 5)["runs"][0].as_str().unwrap().to_string();
    assert!(
        f.check_targets(&run)
            .iter()
            .all(|(_, t)| t == "sim_sandbox"),
        "candidate code never runs on the Mac"
    );

    // The caps: one self PR a day by default, so the next shift takes no task.
    self_issue(&f, 7, "SELF-DOCS Another one.", "pxm-0");
    let out = f.shift();
    let id = out["shift"].as_str().unwrap();
    let journal = f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(id)
            .join("journal.jsonl"),
    );
    assert!(
        journal.iter().any(|e| e["kind"] == "self_lane"
            && e["why"]
                .as_str()
                .is_some_and(|w| w.contains("max_self_prs_per_day"))),
        "{journal:?}"
    );
    assert_eq!(f.prs().len(), 1);
}

#[test]
fn a_self_plan_naming_a_protected_path_ends_before_any_edit() {
    let Some(cli) = cli() else { return };
    let f = self_fx("selfprotected", &cli, &[SELF]);
    self_issue(&f, 5, "Make add add.", "pxm-0");
    f.shift();
    assert!(f.prs().is_empty());
    assert!(f.labels(SELF, 5).contains(&"wg:stuck".to_string()));
    let comment = f.comments(SELF, 5).join("\n");
    assert!(
        comment.contains("the self plan names protected paths"),
        "{comment}"
    );
    let run = f.record(SELF, 5)["runs"][0].as_str().unwrap().to_string();
    let calls: Vec<String> = f
        .run_journal(&run)
        .iter()
        .filter(|e| e["kind"] == "command" && e["what"] == "llm")
        .map(|e| e["label"].as_str().unwrap().to_string())
        .collect();
    assert!(calls.iter().all(|l| !l.starts_with("work:")), "{calls:?}");
}

#[test]
fn a_factory_proposal_is_filed_once_and_never_worked() {
    let Some(cli) = cli() else { return };
    let f = self_fx("selfproposal", &cli, &["o/r", SELF]);
    std::fs::write(f.home.join("factory-proposal"), "").unwrap();
    // FAIL-CHECK leaves the unit stuck, so the learn step runs (it needs evidence).
    f.issue(
        "o/r",
        1,
        "Fix add",
        "FAIL-CHECK add subtracts",
        "pxm-0",
        "pxm-0",
    );
    let first = f.shift();
    let first_journal = f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(first["shift"].as_str().unwrap())
            .join("journal.jsonl"),
    );
    let filed: Vec<(String, Value)> = f.state()["issues"][SELF]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(filed.len(), 1, "{first_journal:?}");
    let (n, issue) = &filed[0];
    assert!(
        issue["labels"].to_string().contains("wg:self-proposed"),
        "{issue}"
    );
    assert!(issue["title"].as_str().unwrap().starts_with("factory: "));
    // Even if someone labels it wg:self, the lane never takes an issue the factory created.
    let mut s = f.state();
    s["issues"][SELF][n]["labels"] = json!([{ "name": "wg:self" }]);
    s["issues"][SELF][n]["events"] =
        json!([{ "label": "wg:self", "actor": "pxm-0", "at": "2026-11-01T11:00:00Z" }]);
    f.set_state(&s);
    f.issue(
        "o/r",
        2,
        "Fix add again",
        "FAIL-CHECK add subtracts",
        "pxm-0",
        "pxm-0",
    );
    let out = f.shift();
    let id = out["shift"].as_str().unwrap();
    let journal = f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(id)
            .join("journal.jsonl"),
    );
    assert!(
        journal
            .iter()
            .any(|e| e["kind"] == "issue_skipped" && e["why"] == "created by the factory"),
        "{journal:?}"
    );
    assert_eq!(
        f.state()["issues"][SELF].as_object().unwrap().len(),
        1,
        "the same proposal is never filed twice"
    );
    assert!(f.prs().iter().all(|p| p["repo"] != SELF));
}
