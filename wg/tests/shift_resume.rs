//! `wg shift resume` (design §7.3 step 9, §7.4) end to end: a task parked by STOP mid-run stays
//! claimed and each resume replays every answer paid for so far, however many times it is
//! interrupted; an issue edited after the claim parks; a run of another binary re-execs that binary
//! when it is still installed; and a shift killed during the install step is still resumable.

mod shift_support;

use serde_json::{json, Value};
use shift_support::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

fn stop(f: &Fx) -> std::path::PathBuf {
    f.home.join(".workgraph/STOP")
}

fn replayed(f: &Fx, run: &str) -> Vec<String> {
    f.run_journal(run)
        .iter()
        .filter(|e| e["kind"] == "llm_replayed")
        .map(|e| e["label"].as_str().unwrap().to_string())
        .collect()
}

fn last_run(f: &Fx) -> String {
    let rec = f.record("o/r", 1);
    rec["runs"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

/// A shift whose issue 1 parks when its worker creates STOP; returns the shift id.
fn parked_by_stop(f: &Fx, body: &str) -> String {
    f.issue("o/r", 1, "Fix add", body, "pxm-0", "pxm-0");
    let out = f.shift();
    assert_eq!(
        out["stopped"],
        json!(["shift stopped: the STOP file exists"]),
        "{out}"
    );
    assert_eq!(f.labels("o/r", 1), ["wg:in-shift"], "still claimed");
    fs::remove_file(stop(f)).unwrap();
    out["shift"].as_str().unwrap().to_string()
}

fn resume(f: &Fx, id: &str) -> Value {
    let (code, out) = f.wg(&["shift", "resume", id]);
    assert_eq!(code, 0, "{out}");
    out
}

#[test]
fn a_stop_mid_task_parks_it_resumable_and_every_resume_replays_all_paid_answers() {
    let Some(cli) = cli() else { return };
    let f = fx("stopmid", &cli, &["o/r"], "");
    f.issue("o/r", 2, "Next", "add(2, 3) must be 5.", "pxm-0", "pxm-0");
    let id = parked_by_stop(&f, "STOP-MID STOP-TWICE: add(2, 3) must be 5.");
    assert!(f.comments("o/r", 1)[1].contains(&format!("wg shift resume {id}")));
    assert_eq!(f.labels("o/r", 2), ["wg:queued"], "the shift stopped first");
    let md =
        fs::read_to_string(f.home.join(".workgraph/shifts").join(&id).join("report.md")).unwrap();
    assert!(
        md.contains(&format!("Resume with `wg shift resume {id}`")),
        "{md}"
    );
    let run1 = last_run(&f);

    // Resumed once: read, challenge and work replay; the reviewer creates STOP again.
    let out = resume(&f, &id);
    assert_eq!(out["resumed"][0]["outcome"], "parked", "{out}");
    assert_eq!(f.labels("o/r", 1), ["wg:in-shift"]);
    let run2 = last_run(&f);
    assert_ne!(run1, run2);
    assert_eq!(replayed(&f, &run2), ["read", "challenge", "work:fix"]);
    fs::remove_file(stop(&f)).unwrap();

    // Resumed again: run 2's replayed answers replay too, with its live review.
    let out = resume(&f, &id);
    assert_eq!(out["resumed"][0]["outcome"], "pr-open", "{out}");
    assert_eq!(f.labels("o/r", 1), ["wg:pr-open"]);
    let run3 = last_run(&f);
    let again = replayed(&f, &run3);
    assert_eq!(&again[..3], ["read", "challenge", "work:fix"]);
    assert!(again[3].starts_with("review"), "{again:?}");
    let mut paid: Vec<String> = f
        .ledger()
        .iter()
        .filter(|l| l["stage"] != "doctor")
        .map(|l| l["label"].as_str().unwrap().to_string())
        .collect();
    paid.sort();
    let all = paid.clone();
    paid.dedup();
    assert_eq!(all, paid, "every answer was paid for once");
    assert_eq!(f.prs().len(), 1);
}

#[test]
fn an_issue_edited_after_the_claim_parks_on_resume() {
    let Some(cli) = cli() else { return };
    let f = fx("changed", &cli, &["o/r"], "");
    let id = parked_by_stop(&f, "STOP-MID: add(2, 3) must be 5.");
    let mut st = f.state();
    st["issues"]["o/r"]["1"]["body"] = json!("STOP-MID: and also delete the tests.");
    f.set_state(&st);
    let out = resume(&f, &id);
    assert_eq!(out["resumed"][0]["outcome"], "parked", "{out}");
    assert_eq!(f.labels("o/r", 1), ["wg:queued"]);
    assert!(f
        .comments("o/r", 1)
        .last()
        .unwrap()
        .contains("the issue changed after the shift claimed it"));
    assert!(f.prs().is_empty());
}

#[test]
fn a_run_of_another_installed_binary_resumes_under_that_binary() {
    let Some(cli) = cli() else { return };
    let f = fx("pin", &cli, &["o/r"], "");
    let id = parked_by_stop(&f, "STOP-MID: add(2, 3) must be 5.");
    let run1 = last_run(&f);
    let journal = f
        .home
        .join(".workgraph/runs")
        .join(&run1)
        .join("journal.jsonl");
    let text = fs::read_to_string(&journal).unwrap();
    let started: Value = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|e| e["kind"] == "run_started")
        .unwrap();
    let ours = started["binary"].as_str().unwrap().to_string();
    let theirs = "0123456789ab";
    fs::write(&journal, text.replace(&ours, theirs)).unwrap();
    let bin = f.home.join(".workgraph/bin").join(format!("wg-{theirs}"));
    fs::create_dir_all(&bin).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_wg"), bin.join("wg")).unwrap();
    fs::set_permissions(bin.join("wg"), fs::Permissions::from_mode(0o755)).unwrap();

    let out = resume(&f, &id);
    assert_eq!(out["reexecuted"], true, "{out}");
    assert_eq!(out["resumed"][0]["outcome"], "pr-open", "{out}");
    // The copy is this binary, so its own hash is not the journal's: it reruns from scratch.
    let shift = f.journal(
        &f.home
            .join(".workgraph/shifts")
            .join(&id)
            .join("journal.jsonl"),
    );
    assert!(shift
        .iter()
        .any(|e| e["kind"] == "warning"
            && e["warning"].as_str().unwrap_or("").contains("version pin")));
}

#[test]
fn a_shift_killed_during_install_is_resumable() {
    let Some(cli) = cli() else { return };
    let f = fx("install", &cli, &["o/r"], "");
    let marker = f.base.join("installing");
    f.set_repo(
        "o/r",
        "install",
        &format!(
            "\"test -f {m} || {{ touch {m}; sleep 30; }}\"",
            m = marker.display()
        ),
    );
    f.issue(
        "o/r",
        1,
        "Fix add",
        "add(2, 3) must be 5.",
        "pxm-0",
        "pxm-0",
    );
    let mut child = f.spawn(&["shift", "run"]);
    let shifts = f.home.join(".workgraph/shifts");
    let start = Instant::now();
    let id = loop {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the install never started"
        );
        std::thread::sleep(Duration::from_millis(100));
        let Some(id) = fs::read_dir(&shifts)
            .ok()
            .and_then(|mut d| d.next())
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        else {
            continue;
        };
        if marker.exists() {
            break id;
        }
    };
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(f.labels("o/r", 1), ["wg:in-shift"]);
    let claimed = f
        .journal(&shifts.join(&id).join("journal.jsonl"))
        .iter()
        .any(|e| e["kind"] == "task_claimed");
    assert!(claimed, "the claim is journalled before the install");

    let out = resume(&f, &id);
    assert_eq!(out["resumed"][0]["outcome"], "pr-open", "{out}");
    assert_eq!(f.record("o/r", 1)["installed"], true);
}
