//! The finalize chain: commit, record the build gate, verify, record the verification gate and
//! commit the evidence. Stops at the first failing step.

use crate::exec::run::tail;
use serde_json::{json, Map, Value};
use std::fs;
use std::process::Command;

/// Validated finalize inputs; `cli` is absolute and `staged` is non-empty.
pub struct Finalize {
    pub task: String,
    pub by: String,
    pub cli: String,
    pub message: String,
    pub evidence_message: String,
    pub staged: Vec<String>,
}

/// Run one finalize step and record its exit code and output tail.
fn step(steps: &mut Vec<Value>, name: &str, program: &str, args: &[&str]) -> bool {
    let (ok, code, text) = match Command::new(program).args(args).output() {
        Ok(o) => (
            o.status.success(),
            o.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (false, 127, format!("failed to start {program}: {e}")),
    };
    steps.push(json!({ "step": name, "ok": ok, "exitCode": code, "outputTail": tail(&text) }));
    ok
}

fn set_task_status(steps: &mut Vec<Value>, task: &str, status: &str) -> bool {
    let path = format!(".haruspex/tasks/{task}.json");
    let result = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {path}: {e}"))
        .and_then(|text| {
            serde_json::from_str::<Value>(&text)
                .map_err(|e| format!("{path} is not valid JSON: {e}"))
        })
        .and_then(|mut v| {
            let obj: &mut Map<String, Value> = v
                .as_object_mut()
                .ok_or(format!("{path} is not an object"))?;
            obj.insert("status".into(), Value::String(status.into()));
            fs::write(
                &path,
                serde_json::to_string_pretty(&v).expect("Value serializes") + "\n",
            )
            .map_err(|e| format!("cannot write {path}: {e}"))
        });
    steps.push(json!({ "step": format!("status {status}"), "ok": result.is_ok(), "outputTail": result.err().unwrap_or_default() }));
    steps.last().is_some_and(|s| s["ok"] == true)
}

/// Runs every step in order and reports them as `{passed, blockedAt, staged, steps}`.
pub fn finalize_chain(f: &Finalize) -> Value {
    let (task, by, cli) = (f.task.as_str(), f.by.as_str(), f.cli.as_str());
    let mut s = Vec::new();
    let mut git_add = vec!["add", "--"];
    git_add.extend(f.staged.iter().map(String::as_str));
    let note = "Approved after the autonomous build under the auto-proceed policy";
    let passed = step(&mut s, "stage", "git", &git_add)
        && step(&mut s, "commit", "git", &["commit", "-m", &f.message])
        && step(
            &mut s,
            "approve ready-to-build",
            "python3",
            &[
                cli,
                "approve",
                "ready-to-build",
                ".",
                "--by",
                by,
                "--note",
                note,
            ],
        )
        && step(
            &mut s,
            "gate ready-to-build",
            "python3",
            &[cli, "gate", "ready-to-build", ".", "--record"],
        )
        && set_task_status(&mut s, task, "verifying")
        && step(&mut s, "verify", "python3", &[cli, "verify", "."])
        && step(
            &mut s,
            "gate verification",
            "python3",
            &[cli, "gate", "verification", ".", "--record"],
        )
        && step(&mut s, "stage evidence", "git", &["add", "--", ".haruspex"])
        && step(
            &mut s,
            "commit evidence",
            "git",
            &["commit", "-m", &f.evidence_message],
        );
    let blocked_at = s
        .iter()
        .find(|x| x["ok"] == false)
        .map(|x| x["step"].clone());
    json!({ "passed": passed, "blockedAt": blocked_at, "staged": f.staged, "steps": s })
}

/// What `finalize_run` needs beyond the approval: the commit messages, the pinned CLI, the
/// changed paths to stage (the user's own work already removed) and the check pins to re-read.
pub struct RunFinalize<'a> {
    pub cli: &'a str,
    pub message: &'a str,
    pub evidence_message: &'a str,
    pub staged: Vec<String>,
    pub pins: &'a std::collections::BTreeMap<String, String>,
}

/// `project.json` commands still equal the pins taken at Read (design §9.3).
fn pins_hold(steps: &mut Vec<Value>, pins: &std::collections::BTreeMap<String, String>) -> bool {
    let result = fs::read_to_string(".haruspex/project.json")
        .map_err(|e| format!("cannot read project.json: {e}"))
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
        .and_then(|v| {
            match pins
                .iter()
                .find(|(name, cmd)| v["commands"][name.as_str()].as_str() != Some(cmd.as_str()))
            {
                Some((name, _)) => Err(format!("check {name} changed in project.json since Read")),
                None => Ok(()),
            }
        });
    steps.push(json!({ "step": "check pins", "ok": result.is_ok(), "outputTail": result.err().unwrap_or_default() }));
    steps.last().is_some_and(|s| s["ok"] == true)
}

/// The run's finalize chain (design §9.5), callable only with the owner's `HumanApproval`. Unlike
/// the legacy chain it tolerates a clean tree: it commits only when something is staged.
pub fn finalize_run(approval: &crate::tokens::HumanApproval, f: &RunFinalize) -> Value {
    let (task, by, cli) = (approval.task(), approval.by(), f.cli);
    let mut s = Vec::new();
    let note = format!(
        "approved by {by} via /workgraph:work approve after the build, under the auto-proceed policy; head {}",
        &approval.head()[..approval.head().len().min(12)]
    );
    let committed = if f.staged.is_empty() {
        true
    } else {
        let mut git_add = vec!["add", "--"];
        git_add.extend(f.staged.iter().map(String::as_str));
        step(&mut s, "stage", "git", &git_add)
            && step(&mut s, "commit", "git", &["commit", "-m", f.message])
    };
    let passed = committed
        && step(
            &mut s,
            "approve ready-to-build",
            "python3",
            &[
                cli,
                "approve",
                "ready-to-build",
                ".",
                "--by",
                by,
                "--note",
                &note,
            ],
        )
        && step(
            &mut s,
            "gate ready-to-build",
            "python3",
            &[cli, "gate", "ready-to-build", ".", "--record"],
        )
        && set_task_status(&mut s, task, "verifying")
        && pins_hold(&mut s, f.pins)
        && step(&mut s, "verify", "python3", &[cli, "verify", "."])
        && step(
            &mut s,
            "gate verification",
            "python3",
            &[cli, "gate", "verification", ".", "--record"],
        )
        && step(&mut s, "stage evidence", "git", &["add", "--", ".haruspex"])
        && step(
            &mut s,
            "commit evidence",
            "git",
            &["commit", "-m", f.evidence_message],
        );
    let blocked_at = s
        .iter()
        .find(|x| x["ok"] == false)
        .map(|x| x["step"].clone());
    json!({ "passed": passed, "blockedAt": blocked_at, "staged": f.staged, "steps": s })
}
