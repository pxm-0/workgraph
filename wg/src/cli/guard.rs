//! `wg guard --manifest <path>`: the PreToolUse hook entry point. It fails closed: every error,
//! every caught panic and an internal 5 s deadline exit 2 with a deny (design §5.4). Claude Code
//! treats any other exit code as a non-blocking error and runs the tool anyway (ADR-001 §12).

use crate::guard::hook::{decide, Manifest, Verdict};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(5);

/// Runs the hook and returns the process exit code: 0 allows, 2 denies.
pub fn main(args: &[String]) -> i32 {
    let outcome = guarded(DEADLINE, {
        let args = args.to_vec();
        move || handle(&args)
    });
    match outcome {
        Ok(()) => 0,
        Err(reason) => {
            deny(&reason);
            2
        }
    }
}

fn deny(reason: &str) {
    let out = json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse",
        "permissionDecision": "deny", "permissionDecisionReason": reason } });
    // Best effort: the exit code 2 blocks the call even if these writes fail.
    let _ = writeln!(std::io::stdout(), "{out}");
    let _ = writeln!(std::io::stderr(), "wg guard: {reason}");
}

/// Runs `f` on its own thread with a deadline; a panic or the deadline is an `Err`.
pub fn guarded<F>(deadline: Duration, f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .unwrap_or_else(|_| Err("the guard panicked; denying".to_string()));
        let _ = tx.send(r);
    });
    rx.recv_timeout(deadline)
        .unwrap_or_else(|_| Err("the guard missed its deadline; denying".to_string()))
}

fn handle(args: &[String]) -> Result<(), String> {
    // Input first: a guard that denies before reading leaves Claude Code's write with EPIPE.
    let mut input = String::new();
    std::io::stdin()
        .take(4 * 1024 * 1024)
        .read_to_string(&mut input)
        .map_err(|e| format!("cannot read the hook input: {e}"))?;
    #[cfg(debug_assertions)]
    if std::env::var_os("WG_GUARD_TEST_PANIC").is_some() {
        panic!("forced by WG_GUARD_TEST_PANIC (debug builds only)");
    }
    let path = match args {
        [flag, p] if flag == "--manifest" => p,
        _ => return Err("usage: wg guard --manifest <path>".into()),
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read manifest: {e}"))?;
    let m: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("the manifest is not valid: {e}"))?;
    let hook: Value =
        serde_json::from_str(&input).map_err(|e| format!("the hook input is not JSON: {e}"))?;
    let verdict = decide(&m, &hook);
    log(&m, &hook, &verdict)?;
    match verdict {
        Verdict::Allow { .. } => Ok(()),
        Verdict::Deny(why) => Err(why),
        Verdict::Incident(why) => Err(format!("incident: {why}")),
    }
}

/// What the call targeted: the Bash command, the file or the skill, for the owner's audit.
fn input_summary(input: &Value) -> String {
    [
        "command",
        "file_path",
        "notebook_path",
        "path",
        "pattern",
        "skill",
    ]
    .iter()
    .find_map(|k| input[*k].as_str())
    .unwrap_or("")
    .to_string()
}

/// Appends the decision to `guard.jsonl`; a log that cannot be written denies the call.
fn log(m: &Manifest, hook: &Value, v: &Verdict) -> Result<(), String> {
    let (decision, reason, effect) = match v {
        Verdict::Allow { effect } => ("allow", String::new(), *effect),
        Verdict::Deny(r) => ("deny", r.clone(), false),
        Verdict::Incident(r) => ("incident", r.clone(), false),
    };
    let line = json!({
        "ts": crate::journal::now(),
        "label": m.label,
        "tool": hook["tool_name"],
        "input": crate::ids::one_line(&input_summary(&hook["tool_input"]), 300),
        "decision": decision,
        "effect": effect,
        "reason": reason,
    });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&m.log)
        .map_err(|e| format!("cannot open the guard log: {e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("cannot write the guard log: {e}"))
}

#[cfg(test)]
mod tests {
    use super::guarded;
    use std::time::Duration;

    #[test]
    fn a_panic_or_a_missed_deadline_is_a_deny() {
        assert_eq!(guarded(Duration::from_secs(1), || Ok(())), Ok(()));
        let panicked = guarded(Duration::from_secs(1), || panic!("boom"));
        assert!(panicked.unwrap_err().contains("panicked"));
        let slow = guarded(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        });
        assert!(slow.unwrap_err().contains("deadline"));
        let err = guarded(Duration::from_secs(1), || Err("no".into()));
        assert_eq!(err, Err("no".into()));
    }
}
