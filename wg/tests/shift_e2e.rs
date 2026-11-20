//! `wg shift run` end to end (design §15.4): two toy repos with local bare origins, a scripted
//! claude, a fake gh and the real Haruspex CLI. Label transitions, the draft PR, the
//! needs-decision comment and the decide round trip, the report and its spend, resume after a
//! kill mid-stage, STOP, max_tasks, a rate limit with no fallback, and an untrusted labeller.

mod shift_support;

use serde_json::json;
use shift_support::*;
use std::time::{Duration, Instant};

#[test]
fn a_shift_turns_queued_issues_into_draft_prs_decisions_and_a_report() {
    let Some(cli) = cli() else { return };
    let f = fx("main", &cli, &["o/r", "o/s"], "");
    f.issue(
        "o/r",
        1,
        "Fix add so it adds",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    f.issue("o/r", 2, "Sneaky", "do something", "pxm-0", "stranger");
    f.issue(
        "o/r",
        3,
        "Rework storage",
        "NEEDS-DECISION: also clean the old table.",
        "pxm-0",
        "pxm-0",
    );
    f.issue(
        "o/s",
        4,
        "Fix add in s",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    let out = f.shift();
    let id = out["shift"].as_str().unwrap().to_string();
    assert_eq!(out["tasks"], 3, "{out}");
    assert_eq!(
        out["needsYou"],
        json!({ "incidents": 0, "decisions": 1, "approvals": 2, "parked": 0 })
    );

    // Labels: queued → in-shift → outcome; the untrusted labeller's issue is untouched.
    assert_eq!(f.labels("o/r", 1), ["wg:pr-open"]);
    assert_eq!(f.labels("o/r", 2), ["wg:queued"]);
    assert_eq!(f.labels("o/r", 3), ["wg:needs-decision"]);
    assert_eq!(f.labels("o/s", 4), ["wg:pr-open"]);
    let log = f.state()["log"].clone();
    let edits: Vec<String> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a[0] == "issue" && a[1] == "edit" && a[2] == "1")
        .map(|a| a.to_string())
        .collect();
    assert!(
        edits[0].contains("--add-label\",\"wg:in-shift\",\"--remove-label\",\"wg:queued"),
        "{edits:?}"
    );
    assert!(
        edits[1].contains("--add-label\",\"wg:pr-open\",\"--remove-label\",\"wg:in-shift"),
        "{edits:?}"
    );

    // The draft PR: pushed task branch, kernel-written body, the stage's label.
    let prs = f.prs();
    assert_eq!(prs.len(), 2);
    let pr = &prs[0];
    assert_eq!(
        (pr["repo"].as_str(), pr["draft"].as_bool()),
        (Some("o/r"), Some(true))
    );
    assert_eq!(pr["headRefName"], "wg/1-fix-add-so-it-adds");
    assert_eq!(pr["title"], "feat: Fix add so it adds");
    assert_eq!(pr["labels"], json!(["wg:awaiting-approval"]));
    let body = pr["body"].as_str().unwrap();
    assert!(
        body.starts_with("Closes #1")
            && body.contains("Gate checks, passing at the current tree: test"),
        "{body}"
    );
    let msg = sh(
        &f.origin("o/r"),
        "git log -1 --format=%B refs/heads/wg/1-fix-add-so-it-adds",
    );
    assert!(
        msg.starts_with("feat: Fix add so it adds")
            && msg.contains("Workgraph-Run: r-")
            && msg.contains("Co-Authored-By: Claude Opus 5.5"),
        "{msg}"
    );
    assert!(f.comments("o/r", 1)[0].contains(&format!("shift `{id}` picked this up")));
    assert!(f.comments("o/r", 1)[1].starts_with("Draft PR: https://github.com/o/r/pull/100"));
    let run1 = f.record("o/r", 1)["runs"][0].as_str().unwrap().to_string();
    let output: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            f.home
                .join(".workgraph/runs")
                .join(&run1)
                .join("output.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(output["pr"], "https://github.com/o/r/pull/100");
    assert!(output["next"]
        .as_str()
        .unwrap()
        .contains("/workgraph:work approve"));

    // The needs-decision comment: the question fenced as data, recommendation first.
    let ask = &f.comments("o/r", 3)[1];
    assert!(
        ask.contains("**E-CRIT** (critical). Recommended: ```text\nKeep it\n```"),
        "{ask}"
    );
    assert!(
        ask.contains("```text\nDrop the legacy table?\n```")
            && ask.contains("/workgraph:shift decide o/r#3")
    );

    // The owner's checkout was never touched.
    assert_eq!(sh(&f.repo("o/r"), "git status --porcelain"), "");
    assert_eq!(sh(&f.repo("o/r"), "git branch --show-current"), "main");

    // The report: needs-you first, skipped issues, spend per call, stage, task and provider.
    let r = f.report(&id);
    assert_eq!(r["needsYou"]["decisions"][0]["issue"], 3);
    assert_eq!(r["needsYou"]["approvals"][0]["verified"], true);
    assert_eq!(r["needsYou"]["approvals"][0]["reviewed"], true);
    let a = &r["needsYou"]["approvals"][0];
    assert_eq!(
        (a["title"].as_str(), a["pr"].as_u64()),
        (Some("Fix add so it adds"), Some(100))
    );
    assert_eq!(
        a["criteria"][0],
        json!({ "id": "AC-001", "status": "passed", "evidence": ["check:test"] })
    );
    assert!(r["skippedIssues"][0]["why"]
        .as_str()
        .unwrap()
        .contains("labelled wg:queued by stranger"));
    let spend = &r["spend"];
    // read, challenge, work, review; learn too when a coverage build leaves profile files behind.
    let task_calls = spend["byTask"]["o/r#1/subscription"]["calls"]
        .as_u64()
        .unwrap();
    assert!((4..=5).contains(&task_calls), "{spend}");
    assert_eq!(spend["byStage"]["read/subscription"]["calls"], 3);
    assert!(
        spend["byProvider"]["subscription/opus"]["calls"]
            .as_u64()
            .unwrap()
            >= 5
    );
    assert_eq!(spend["callsWithUnknownCost"], 0);
    assert!(spend["usd"].as_f64().unwrap() > 0.0);
    let md = std::fs::read_to_string(f.home.join(".workgraph/shifts").join(&id).join("report.md"))
        .unwrap();
    assert!(md.find("### Incidents").unwrap() < md.find("### Decisions").unwrap());
    assert!(md.contains("/workgraph:shift decide o/r#3"));
    assert!(
        md.contains("- #100 Fix add so it adds (o/r#1)")
            && md.contains("AC-001 passed")
            && md.contains("`/workgraph:work approve o/r#100`"),
        "{md}"
    );

    // The decide round trip: the answer is stored, the issue queued, and the next shift ships it.
    let answers = f.base.join("answers.json");
    std::fs::write(&answers, r#"[{"id":"E-CRIT","answer":"Keep it"}]"#).unwrap();
    let (code, d) = f.wg(&[
        "decide",
        "--repo",
        "o/r",
        "--issue",
        "3",
        "--decisions-file",
        answers.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{d}");
    assert_eq!(f.labels("o/r", 3), ["wg:queued"]);
    let next = f.shift();
    assert_eq!(next["tasks"], 1, "{next}");
    assert_eq!(f.labels("o/r", 3), ["wg:pr-open"]);
    let rec = f.record("o/r", 3);
    assert_eq!(
        rec["runs"].as_array().unwrap().len(),
        2,
        "a rerun links both runs"
    );
    assert_eq!(rec["task_id"], "T-001");
    assert_eq!(
        rec["branch"], "wg/3-rework-storage",
        "the kept branch is reused"
    );
}

#[test]
fn resume_after_a_kill_mid_stage_replays_what_the_run_already_got() {
    let Some(cli) = cli() else { return };
    let f = fx("resume", &cli, &["o/r"], "");
    f.issue(
        "o/r",
        1,
        "Fix add slowly",
        "SLOW: add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    let mut child = f.spawn(&["shift", "run"]);
    let shifts = f.home.join(".workgraph/shifts");
    let start = Instant::now();
    let (id, run) = loop {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the worker never started"
        );
        std::thread::sleep(Duration::from_millis(100));
        let Some(id) = std::fs::read_dir(&shifts)
            .ok()
            .and_then(|mut d| d.next())
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        else {
            continue;
        };
        let claimed = f
            .journal(&shifts.join(&id).join("journal.jsonl"))
            .into_iter()
            .find(|e| e["kind"] == "task_claimed");
        let Some(run) = claimed.and_then(|e| e["run"].as_str().map(str::to_string)) else {
            continue;
        };
        if f.run_journal(&run)
            .iter()
            .any(|e| e["kind"] == "llm_started" && e["label"] == "work:fix")
        {
            break (id, run);
        }
    };
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        f.labels("o/r", 1),
        ["wg:in-shift"],
        "killed mid-stage: still claimed"
    );

    let (code, out) = f.wg(&["shift", "resume", &id]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["resumed"][0]["outcome"], "pr-open", "{out}");
    assert_eq!(f.labels("o/r", 1), ["wg:pr-open"]);
    let rec = f.record("o/r", 1);
    let resumed = rec["runs"][1].as_str().unwrap();
    assert_eq!(rec["runs"][0], run);
    let replayed: Vec<String> = f
        .run_journal(resumed)
        .iter()
        .filter(|e| e["kind"] == "llm_replayed")
        .map(|e| e["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(replayed, ["read", "challenge"]);
    let reads = f.ledger().iter().filter(|l| l["label"] == "read").count();
    assert_eq!(reads, 1, "the Reading was not paid for twice");
    assert_eq!(f.prs().len(), 1);
}

#[test]
fn stop_file_max_tasks_and_a_rate_limit_without_fallback_stop_intake() {
    let Some(cli) = cli() else { return };
    let f = fx("stops", &cli, &["o/r"], "max_tasks = 1");
    f.issue("o/r", 1, "First", "add(2, 3) must be 5.", "pxm-0", "pxm-0");
    f.issue("o/r", 2, "Second", "add(2, 3) must be 5.", "pxm-0", "pxm-0");
    std::fs::write(f.home.join(".workgraph/STOP"), "").unwrap();
    let (code, out) = f.wg(&["shift", "run"]);
    assert_eq!(
        (code, out["skipped"].as_str()),
        (0, Some("the STOP file exists"))
    );
    std::fs::remove_file(f.home.join(".workgraph/STOP")).unwrap();
    // A shift that died holding the lock is reported.
    std::fs::write(f.home.join(".workgraph/shift.lock"), "999999999 1\n").unwrap();

    let out = f.shift();
    assert_eq!(out["tasks"], 1);
    assert_eq!(
        f.report(out["shift"].as_str().unwrap())["staleLockPid"],
        999999999
    );
    assert_eq!(out["stopped"], json!(["max_tasks (1) reached"]));
    assert_eq!(f.labels("o/r", 2), ["wg:queued"]);

    let g = fx("ratelimit", &cli, &["o/r"], "");
    g.issue(
        "o/r",
        1,
        "Limited",
        "RATE-LIMIT: add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    g.issue("o/r", 2, "Next", "add(2, 3) must be 5.", "pxm-0", "pxm-0");
    let out = g.shift();
    assert_eq!(out["tasks"], 1, "{out}");
    assert_eq!(
        out["stopped"],
        json!(["rate limited with no usable provider left"])
    );
    assert_eq!(out["needsYou"]["parked"], 1);
    assert_eq!(
        g.labels("o/r", 1),
        ["wg:queued"],
        "parked: back in the queue"
    );
    assert_eq!(g.labels("o/r", 2), ["wg:queued"], "never claimed");
    assert!(g.comments("o/r", 2).is_empty());
}

#[test]
fn a_closed_window_takes_nothing() {
    let Some(cli) = cli() else { return };
    let f = fx("window", &cli, &["o/r"], "");
    let text = std::fs::read_to_string(f.home.join(".workgraph/factory.toml")).unwrap();
    let closed = regex_free_replace_until(&text);
    std::fs::write(f.home.join(".workgraph/factory.toml"), closed).unwrap();
    f.issue("o/r", 1, "First", "x", "pxm-0", "pxm-0");
    let out = f.shift();
    assert_eq!(out["stopped"], json!(["window closed"]));
    assert_eq!(f.labels("o/r", 1), ["wg:queued"]);
}

/// Sets `until` equal to `start`, which closes the window.
fn regex_free_replace_until(toml: &str) -> String {
    let start = toml
        .lines()
        .find_map(|l| l.strip_prefix("start = "))
        .unwrap()
        .to_string();
    toml.lines()
        .map(|l| {
            if l.starts_with("until = ") {
                format!("until = {start}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
