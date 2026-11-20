//! wg: the deterministic half of workgraph. Every subcommand prints one JSON line on stdout.
//! Run it from the repo root.
//!
//!   wg recall
//!   wg check --nonce N [--timeout-secs S] --cmd "npm test" [--cmd …]
//!   wg save < memory.json
//!   wg finalize --task T --by NAME --cli /abs/haruspex.py --message M --evidence-message M2 [--exclude-json '[...]']

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MEMORY: &str = ".claude/work-memory.json";
const TAIL_LINES: usize = 40;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("recall") => recall(),
        Some("check") => check(&args[1..]),
        Some("save") => save(),
        Some("finalize") => finalize(&args[1..]),
        _ => Err("usage: wg recall | check | save | finalize".to_string()),
    };
    match result {
        Ok(v) => println!("{v}"),
        Err(e) => {
            println!("{}", json!({ "error": e }));
            std::process::exit(2);
        }
    }
}

/// `--name value` pairs in order; a flag given twice keeps every value.
fn flags(args: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let name = k.strip_prefix("--").ok_or(format!("unexpected argument: {k}"))?;
        let v = it.next().ok_or(format!("--{name} needs a value"))?;
        out.push((name.to_string(), v.clone()));
    }
    Ok(out)
}
fn last(f: &[(String, String)], name: &str) -> Option<String> {
    f.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}
fn one(f: &[(String, String)], name: &str) -> Result<String, String> {
    last(f, name).ok_or(format!("--{name} is required"))
}

/// Paths from `git status --porcelain=v1 -z`; for renames and copies, the new path.
fn dirty() -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .map_err(|e| format!("git status failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!("git status failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split('\0').filter(|p| !p.is_empty());
    let mut paths = Vec::new();
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        if entry.starts_with('R') || entry.starts_with('C') {
            parts.next(); // skip the original path
        }
        paths.push(entry[3..].to_string());
    }
    Ok(paths)
}

fn recall() -> Result<Value, String> {
    let dirty = dirty()?;
    match fs::read_to_string(MEMORY) {
        Ok(content) => Ok(json!({ "exists": true, "content": content, "dirty": dirty })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({ "exists": false, "dirty": dirty })),
        Err(e) => Err(format!("cannot read {MEMORY}: {e}")),
    }
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

/// Run one shell command with stdout and stderr interleaved; kill its whole process group on timeout.
fn run_shell(cmd: &str, timeout: Duration) -> (i64, String) {
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!("{{ {cmd}\n}} 2>&1"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (127, format!("failed to start: {e}")),
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().map(i64::from).unwrap_or(-1),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = Command::new("kill").args(["-9", &format!("-{}", child.id())]).status();
                let _ = child.wait();
                timed_out = true;
                break 124;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break -1,
        }
    };
    let mut out = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
    if timed_out {
        out.push_str(&format!("\n[wg: timed out after {}s]", timeout.as_secs()));
    }
    (code, tail(&out))
}

fn check(args: &[String]) -> Result<Value, String> {
    let f = flags(args)?;
    let nonce = one(&f, "nonce")?;
    let secs: u64 = match last(&f, "timeout-secs") {
        Some(v) => v.parse().map_err(|_| "--timeout-secs must be a whole number".to_string())?,
        None => 600,
    };
    let results: Vec<Value> = f
        .iter()
        .filter(|(k, _)| k == "cmd")
        .map(|(_, c)| {
            let (code, out) = run_shell(c, Duration::from_secs(secs));
            json!({ "cmd": c, "exitCode": code, "outputTail": out })
        })
        .collect();
    Ok(json!({ "nonce": nonce, "results": results, "changedFiles": dirty()? }))
}

fn is_rule(r: &Value) -> bool {
    let positive = |k: &str| r.get(k).and_then(Value::as_f64).is_some_and(|x| x.is_finite() && x > 0.0);
    ["id", "text", "sig"].iter().all(|k| r.get(*k).is_some_and(Value::is_string))
        && matches!(r.get("scope").and_then(Value::as_str), Some("planner" | "worker" | "all"))
        && positive("a")
        && positive("b")
}

fn validate_memory(m: &Value) -> Result<(), String> {
    let o = m.as_object().ok_or("memory must be a JSON object")?;
    if o.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("memory.version must be 1".into());
    }
    for k in ["runs", "nextId"] {
        if !o.get(k).is_some_and(Value::is_u64) {
            return Err(format!("memory.{k} must be a non-negative integer"));
        }
    }
    if !o.get("checks").and_then(Value::as_array).is_some_and(|a| a.iter().all(Value::is_string)) {
        return Err("memory.checks must be an array of strings".into());
    }
    if !o.get("rules").and_then(Value::as_array).is_some_and(|a| a.iter().all(is_rule)) {
        return Err("memory.rules must be an array of valid rules".into());
    }
    if !o.get("models").is_some_and(Value::is_object) {
        return Err("memory.models must be an object".into());
    }
    Ok(())
}

fn save() -> Result<Value, String> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).map_err(|e| format!("cannot read stdin: {e}"))?;
    let memory: Value = serde_json::from_str(&input).map_err(|e| format!("stdin is not JSON: {e}"))?;
    validate_memory(&memory)?;
    if let Ok(existing) = fs::read_to_string(MEMORY) {
        // Never overwrite a memory file we can't parse: it may hold rules worth keeping.
        serde_json::from_str::<Value>(&existing).map_err(|e| format!("{MEMORY} exists but is not valid JSON ({e}); left untouched"))?;
        fs::write(format!("{MEMORY}.bak"), &existing).map_err(|e| format!("cannot write backup: {e}"))?;
    }
    fs::create_dir_all(".claude").map_err(|e| format!("cannot create .claude: {e}"))?;
    let tmp = format!("{MEMORY}.tmp.{}", std::process::id());
    fs::write(&tmp, serde_json::to_string_pretty(&memory).expect("Value serializes") + "\n").map_err(|e| format!("cannot write {tmp}: {e}"))?;
    fs::rename(&tmp, MEMORY).map_err(|e| format!("cannot replace {MEMORY}: {e}"))?;
    Ok(json!({ "saved": true, "path": MEMORY, "rules": memory["rules"].as_array().map_or(0, Vec::len) }))
}

/// Run one finalize step and record its exit code and output tail.
fn step(steps: &mut Vec<Value>, name: &str, program: &str, args: &[&str]) -> bool {
    let (ok, code, text) = match Command::new(program).args(args).output() {
        Ok(o) => (o.status.success(), o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
        Err(e) => (false, 127, format!("failed to start {program}: {e}")),
    };
    steps.push(json!({ "step": name, "ok": ok, "exitCode": code, "outputTail": tail(&text) }));
    ok
}

fn set_task_status(steps: &mut Vec<Value>, task: &str, status: &str) -> bool {
    let path = format!(".haruspex/tasks/{task}.json");
    let result = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {path}: {e}"))
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| format!("{path} is not valid JSON: {e}")))
        .and_then(|mut v| {
            let obj: &mut Map<String, Value> = v.as_object_mut().ok_or(format!("{path} is not an object"))?;
            obj.insert("status".into(), Value::String(status.into()));
            fs::write(&path, serde_json::to_string_pretty(&v).expect("Value serializes") + "\n").map_err(|e| format!("cannot write {path}: {e}"))
        });
    steps.push(json!({ "step": format!("status {status}"), "ok": result.is_ok(), "outputTail": result.err().unwrap_or_default() }));
    steps.last().is_some_and(|s| s["ok"] == true)
}

/// Run only after the user approved the build gate and the commit. Stops at the first failing step.
fn finalize(args: &[String]) -> Result<Value, String> {
    let f = flags(args)?;
    let (task, by, cli) = (one(&f, "task")?, one(&f, "by")?, one(&f, "cli")?);
    let (message, evidence_message) = (one(&f, "message")?, one(&f, "evidence-message")?);
    let exclude: HashSet<String> = match last(&f, "exclude-json") {
        Some(v) => serde_json::from_str::<Vec<String>>(&v).map_err(|e| format!("--exclude-json must be a JSON array of paths: {e}"))?.into_iter().collect(),
        None => HashSet::new(),
    };
    if !Path::new(&cli).is_absolute() {
        return Err("--cli must be an absolute path".into());
    }
    let staged: Vec<String> = dirty()?.into_iter().filter(|p| !exclude.contains(p)).collect();
    if staged.is_empty() {
        return Err("nothing to commit outside the excluded paths".into());
    }

    let mut s = Vec::new();
    let mut git_add = vec!["add", "--"];
    git_add.extend(staged.iter().map(String::as_str));
    let note = "Approved after the autonomous build under the auto-proceed policy";
    let passed = step(&mut s, "stage", "git", &git_add)
        && step(&mut s, "commit", "git", &["commit", "-m", &message])
        && step(&mut s, "approve ready-to-build", "python3", &[&cli, "approve", "ready-to-build", ".", "--by", &by, "--note", note])
        && step(&mut s, "gate ready-to-build", "python3", &[&cli, "gate", "ready-to-build", ".", "--record"])
        && set_task_status(&mut s, &task, "verifying")
        && step(&mut s, "verify", "python3", &[&cli, "verify", "."])
        && step(&mut s, "gate verification", "python3", &[&cli, "gate", "verification", ".", "--record"])
        && step(&mut s, "stage evidence", "git", &["add", "--", ".haruspex"])
        && step(&mut s, "commit evidence", "git", &["commit", "-m", &evidence_message]);
    let blocked_at = s.iter().find(|x| x["ok"] == false).map(|x| x["step"].clone());
    Ok(json!({ "passed": passed, "blockedAt": blocked_at, "staged": staged, "steps": s }))
}
