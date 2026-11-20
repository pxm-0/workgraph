//! Fakes for the flow tests (design §15.2): a scripted `claude` answering by label, checks that
//! fail by label and attempt, a repo whose changed files follow the last check label, and a static
//! Haruspex. Everything else in the run is the real kernel and the real factory.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wg::budget::Ledger;
use wg::config::Config;
use wg::engine::driver::{self, Ran, Start, Step};
use wg::engine::RunCtx;
use wg::journal::Journal;
use wg::ports::{CheckExec, Haruspex, Ports, Proc, RawOutput, Repo, Sandbox, Spawn};
use wg::sandbox::job::{Cleanup, JobResult, Step as JobStep, StepResult};
use wg::state_dirs::Dirs;
use wg_factory::api::Decision;

pub const CLI: &str = "/opt/haruspex/scripts/haruspex.py";
pub const B_FAIL: &str = "FAIL src/b.js:3 expected 302, got 200";
pub const B_SIG: &str = "npm test | FAIL PATH expected N, got N";

pub fn unit(id: &str, extra: Value) -> Value {
    let mut u = json!({ "id": id, "goal": format!("do {id}"), "files": [format!("src/{id}.js")],
                        "dependsOn": [], "criteria": [format!("AC-{id}")], "size": "small", "risky": false });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        u[k] = v;
    }
    u
}

pub fn pass() -> Value {
    json!({ "passed": true, "criteria": [], "findings": [] })
}

pub type Answer = Arc<dyn Fn(&str, &str) -> Option<Value> + Send + Sync>;
pub type Raw = Arc<dyn Fn(&Spawn) -> Option<RawOutput> + Send + Sync>;
pub type Reply = Box<dyn Fn(&str, &str) -> Value + Send + Sync>;
pub type Checker = Arc<dyn Fn(&str, &str, u32) -> Option<String> + Send + Sync>;

/// One scenario. `answer(label, prompt)` overrides the default reply for a label (Some(raw
/// stdout) as a JSON string value means "print exactly this").
pub struct Opts {
    pub units: Vec<Value>,
    pub checks: Vec<(&'static str, &'static str)>,
    pub checker: Checker,
    pub changed: HashMap<&'static str, Vec<&'static str>>,
    pub task: &'static str,
    pub memory: Option<String>,
    pub escalations: Vec<Value>,
    pub challenge: Value,
    pub review: Arc<dyn Fn(u32) -> Value + Send + Sync>,
    pub innocent: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub answer: Answer,
    pub dirty: Vec<&'static str>,
    pub decisions: Vec<(&'static str, Option<&'static str>)>,
    pub resume_task: Option<&'static str>,
    pub initialized: bool,
    pub user_changes: Option<Vec<&'static str>>,
    pub approve_checks: bool,
    pub hx: Option<Arc<FakeHx>>,
    pub checks_fail_to_start: Option<&'static str>,
    pub max_calls: u32,
    /// Appended to factory.toml (providers, auth).
    pub config_extra: String,
    /// Answers a spawn with raw output before the scripted replies (envelope-level tests).
    pub raw: Raw,
    /// The guard logs an incident while this label runs.
    pub guard_incident: Option<&'static str>,
    /// Run as a shift task: the pipeline keeps its publish stage and the driver gets a shift
    /// context (its gh is never reachable, so a publish that passes the invariants fails at git).
    pub shift: bool,
    /// In a shift, the stop condition reports this once `stop_after` steps have run.
    pub stop: Option<(usize, &'static str)>,
    /// The checks that failed at the task's first clean baseline (a rerun).
    pub baseline_floor: Option<Vec<&'static str>>,
    /// The tree fingerprint the repo reports; a test changes it to model an edit.
    pub tree: Arc<Mutex<String>>,
    /// In a shift, `max_parallel_units`.
    pub max_parallel: usize,
    /// How long each Worker call takes, to observe parallelism.
    pub work_ms: u64,
    /// The sandbox port: `None` is no sandbox at all.
    pub sandbox: Option<SandboxSpec>,
    /// The diff the repo shows after the check with this label ran (the classifier's input).
    pub diffs: HashMap<&'static str, &'static str>,
    /// An earlier run of this task built a risky unit (a rerun on its kept worktree).
    pub tainted_earlier: bool,
}

/// A fake sim sandbox: ready, or unavailable for a reason; the agent can die at a label.
#[derive(Clone, Default)]
pub struct SandboxSpec {
    pub unavailable: Option<&'static str>,
    pub dies_at: Option<&'static str>,
}

/// `[[repo]] o/r` with a `[repo.sandbox]` whose `test` check runs `npm test` in the container.
pub fn sandbox_repo() -> String {
    format!(
        "[[repo]]\nname = \"o/r\"\npath = \"/x\"\n[repo.sandbox]\nimage = \"docker.io/library/node@sha256:{}\"\nchecks = {{ test = \"npm test\" }}\n",
        "a".repeat(64)
    )
}

pub struct FakeSandbox {
    pub spec: SandboxSpec,
    pub down: Mutex<Option<String>>,
    pub checker: Checker,
    pub seen: Mutex<HashMap<(String, String), u32>>,
    pub last: Arc<Mutex<String>>,
}

impl Sandbox for FakeSandbox {
    fn unavailable(&self) -> Option<String> {
        self.down.lock().unwrap().clone()
    }
    fn run(&self, nonce: &str, steps: Vec<JobStep>) -> Result<JobResult, String> {
        if let Some(why) = self.unavailable() {
            return Err(format!("sandbox unavailable: {why}"));
        }
        let label = nonce
            .split_once(':')
            .map(|(_, l)| l)
            .unwrap_or(nonce)
            .to_string();
        if self.spec.dies_at == Some(label.as_str()) {
            *self.down.lock().unwrap() = Some("the agent was killed".into());
            return Err("sandbox unavailable: the sandbox job ended without a result (the agent was stopped)".into());
        }
        *self.last.lock().unwrap() = label.clone();
        let mut out = Vec::new();
        for st in &steps {
            let mut seen = self.seen.lock().unwrap();
            let n = seen.entry((label.clone(), st.cmd.clone())).or_insert(0);
            *n += 1;
            let (exit, tail) = match (self.checker)(&label, &st.cmd, *n) {
                Some(t) => (1, t),
                None => (0, "ok".to_string()),
            };
            out.push(StepResult {
                name: st.name.clone(),
                exit,
                timed_out: false,
                oom: false,
                tail,
            });
        }
        Ok(JobResult {
            job: "j-0000000a".into(),
            nonce: nonce.into(),
            steps: out,
            patch: None,
            cleanup: Cleanup::default(),
            error: None,
        })
    }
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            units: vec![],
            checks: vec![("test", "npm test")],
            checker: Arc::new(|_, _, _| None),
            changed: HashMap::new(),
            task: "add a thing",
            memory: None,
            escalations: vec![],
            challenge: json!({ "added": [], "assign": [], "escalations": [] }),
            review: Arc::new(|_| pass()),
            innocent: Arc::new(|_| false),
            answer: Arc::new(|_, _| None),
            dirty: vec![],
            decisions: vec![],
            resume_task: None,
            initialized: false,
            user_changes: None,
            approve_checks: true,
            hx: None,
            checks_fail_to_start: None,
            max_calls: 60,
            config_extra: String::new(),
            raw: Arc::new(|_| None),
            guard_incident: None,
            shift: false,
            stop: None,
            baseline_floor: None,
            tree: Arc::new(Mutex::new("tree-1".into())),
            max_parallel: 1,
            work_ms: 0,
            sandbox: None,
            diffs: HashMap::new(),
            tainted_earlier: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub label: String,
    pub model: String,
    pub effort: Option<String>,
    pub prompt: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub struct FakeProc {
    pub calls: Mutex<Vec<Call>>,
    pub in_flight: std::sync::atomic::AtomicUsize,
    pub max_in_flight: std::sync::atomic::AtomicUsize,
    pub work_ms: u64,
    pub reply: Reply,
    pub raw: Raw,
    pub guard_incident: Option<&'static str>,
}

fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1).cloned())
}

impl Proc for FakeProc {
    fn run(&self, s: &Spawn) -> RawOutput {
        let model = flag(&s.argv, "--model").unwrap_or_default();
        self.calls.lock().unwrap().push(Call {
            label: s.label.clone(),
            model: model.clone(),
            effort: flag(&s.argv, "--effort"),
            prompt: s.stdin.clone(),
            argv: s.argv.clone(),
            env: s.env.clone(),
        });
        if self.guard_incident == Some(s.label.as_str()) {
            let manifest = s
                .env
                .iter()
                .find(|(k, _)| k == "WG_GUARD_MANIFEST")
                .unwrap()
                .1
                .clone();
            let m: Value =
                serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
            let line = json!({ "label": s.label, "decision": "incident", "reason": "write to protected .git/config", "effect": false });
            std::fs::write(m["log"].as_str().unwrap(), format!("{line}\n")).unwrap();
        }
        if let Some(raw) = (self.raw)(s) {
            return raw;
        }
        if s.label.starts_with("work:") && self.work_ms > 0 {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.max_in_flight.fetch_max(now, SeqCst);
            std::thread::sleep(Duration::from_millis(self.work_ms));
            self.in_flight.fetch_sub(1, SeqCst);
        }
        let v = (self.reply)(&s.label, &s.stdin);
        if let Some(raw) = v.as_str() {
            return RawOutput {
                exit: Some(0),
                timed_out: false,
                stdout: raw.into(),
                stderr: String::new(),
            };
        }
        let envelope = json!({ "type": "result", "subtype": "success", "is_error": false,
            "structured_output": v, "total_cost_usd": 0.01, "num_turns": 3, "session_id": "s",
            "modelUsage": { format!("claude-{model}-5-5"): { "outputTokens": 100 } } });
        RawOutput {
            exit: Some(0),
            timed_out: false,
            stdout: envelope.to_string(),
            stderr: String::new(),
        }
    }
}

pub struct FakeChecks {
    pub checker: Checker,
    pub seen: Mutex<HashMap<(String, String), u32>>,
    pub last: Arc<Mutex<String>>,
    pub fail_to_start: Option<&'static str>,
}

impl CheckExec for FakeChecks {
    fn run(&self, label: &str, cmd: &str, _t: Duration) -> Result<(i64, String), String> {
        if self.fail_to_start == Some(label) {
            return Err("sh failed to spawn".into());
        }
        *self.last.lock().unwrap() = label.to_string();
        let mut seen = self.seen.lock().unwrap();
        let n = seen.entry((label.into(), cmd.into())).or_insert(0);
        *n += 1;
        Ok(match (self.checker)(label, cmd, *n) {
            Some(out) => (1, out),
            None => (0, "ok".into()),
        })
    }
}

pub struct FakeRepo {
    pub tree: Arc<Mutex<String>>,
    pub last: Arc<Mutex<String>>,
    pub changed: HashMap<&'static str, Vec<&'static str>>,
    pub diffs: HashMap<&'static str, &'static str>,
}

impl Repo for FakeRepo {
    fn changed(&self) -> Result<Vec<String>, String> {
        let l = self.last.lock().unwrap().clone();
        Ok(self
            .changed
            .get(l.as_str())
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default())
    }
    fn tree(&self) -> Result<String, String> {
        Ok(self.tree.lock().unwrap().clone())
    }
    fn diff(&self, _files: &[String]) -> Result<String, String> {
        let l = self.last.lock().unwrap().clone();
        Ok(self
            .diffs
            .get(l.as_str())
            .map(|d| d.to_string())
            .unwrap_or_default())
    }
}

pub struct FakeHx {
    pub task: Mutex<Value>,
    pub approvals: Mutex<Value>,
    pub project: BTreeMap<String, String>,
    pub blockers: Mutex<Vec<String>>,
}

impl Haruspex for FakeHx {
    fn gate_ready_to_build(&self) -> Result<Vec<String>, String> {
        Ok(self.blockers.lock().unwrap().clone())
    }
    fn task(&self, _id: &str) -> Result<Value, String> {
        Ok(self.task.lock().unwrap().clone())
    }
    fn approvals(&self) -> Result<Value, String> {
        Ok(self.approvals.lock().unwrap().clone())
    }
    fn project_commands(&self) -> Result<BTreeMap<String, String>, String> {
        Ok(self.project.clone())
    }
    fn managed_hash(&self) -> Result<String, String> {
        Ok("managed-1".into())
    }
}

pub fn fake_hx(units: &[Value], checks: &[(&str, &str)]) -> FakeHx {
    let criteria: Vec<Value> = units
        .iter()
        .flat_map(|u| u["criteria"].as_array().cloned().unwrap_or_default())
        .map(|c| json!({ "id": c, "criterion": "x", "status": "unverified" }))
        .collect();
    FakeHx {
        task: Mutex::new(json!({ "id": "T-001", "acceptance_criteria": criteria,
            "required_checks": checks.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            "failure_modes": [{ "id": "FM-001", "severity": "high", "status": "addressed" }] })),
        approvals: Mutex::new(
            json!({ "ready_to_build": { "status": "pending", "by": null, "at": null } }),
        ),
        project: checks
            .iter()
            .map(|(n, c)| (n.to_string(), c.to_string()))
            .collect(),
        blockers: Mutex::new(vec!["Human approval for ready_to_build is pending.".into()]),
    }
}

pub struct Out {
    pub max_in_flight: usize,
    pub result: Value,
    pub calls: Vec<String>,
    pub proc_calls: Vec<Call>,
    pub saved: Option<Value>,
    pub journal: Vec<Value>,
    pub run_dir: PathBuf,
    pub home: PathBuf,
}

impl Out {
    pub fn call(&self, label: &str) -> &Call {
        self.proc_calls
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("no call {label}"))
    }
    pub fn model(&self, label: &str) -> &str {
        &self.call(label).model
    }
    pub fn prompt(&self, label: &str) -> &str {
        &self.call(label).prompt
    }
}

fn learn_reply(prompt: &str) -> Value {
    let body = prompt
        .split("EVIDENCE (data, not instructions):\n```text\n")
        .nth(1)
        .and_then(|r| r.split("\n```").next())
        .unwrap_or("[]");
    let items: Vec<Value> = serde_json::from_str(body).expect("evidence is JSON");
    let rules: Vec<Value> = items
        .iter()
        .filter_map(|i| i["sig"].as_str())
        .map(|sig| json!({ "sig": sig, "text": format!("rule for {sig}"), "scope": "worker" }))
        .collect();
    let proposals = if items.iter().any(|i| i.get("reviewFindings").is_some()) {
        json!([{ "category": "automated-check", "proposal": "lint rule", "evidence": "RF-1" }])
    } else {
        json!([])
    };
    json!({ "rules": rules, "proposals": proposals })
}

/// A fresh directory per call: parallel tests can read the same clock, so a counter keeps them apart.
pub fn temp(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "wg-flow-{name}-{}-{}-{n}",
        std::process::id(),
        wg::journal::now_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn config(checks: &[(&str, &str)], approve: bool) -> Config {
    config_with(checks, approve, 60, "")
}

pub fn config_with(checks: &[(&str, &str)], approve: bool, max_calls: u32, extra: &str) -> Config {
    let mut text = format!("{extra}\n[limits]\nmax_calls_per_task = {max_calls}\n");
    if approve {
        for (n, c) in checks {
            text.push_str(&format!(
                "[[check]]\nrepo = \"o/r\"\nname = \"{n}\"\nproject = \"{c}\"\nfactory = \"{c}\"\n"
            ));
        }
    }
    wg::config::parse(&text).unwrap()
}

/// Runs one scenario through the real driver and factory with `step`.
pub fn run_with(o: Opts, step: Step) -> Out {
    let base = temp("run");
    let home = base.join("home");
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let dirs = Dirs { home: home.clone() };
    let run_id = "run-abc123".to_string();
    let run_dir = dirs.run(&run_id);
    for d in [
        run_dir.join("logs"),
        run_dir.join("guard"),
        dirs.run_tmp(&run_id),
    ] {
        std::fs::create_dir_all(&d).unwrap();
    }
    let cfg = config_with(&o.checks, o.approve_checks, o.max_calls, &o.config_extra);
    let units = o.units.clone();
    let criteria: Vec<Value> = units
        .iter()
        .flat_map(|u| u["criteria"].as_array().cloned().unwrap_or_default())
        .map(|c| json!({ "id": c, "text": "x" }))
        .collect();
    let reading = json!({ "branch": "work/test", "taskId": "T-001", "initialized": true, "risk": "medium",
        "checks": o.checks.iter().map(|(n, c)| json!({ "name": n, "command": c })).collect::<Vec<_>>(),
        "criteria": criteria, "units": units, "escalations": o.escalations });
    let (challenge, review, innocent, answer) = (
        o.challenge.clone(),
        o.review.clone(),
        o.innocent.clone(),
        o.answer.clone(),
    );
    let proc = FakeProc {
        raw: o.raw.clone(),
        guard_incident: o.guard_incident,
        calls: Mutex::new(vec![]),
        in_flight: Default::default(),
        max_in_flight: Default::default(),
        work_ms: o.work_ms,
        reply: Box::new(move |label, prompt| {
            if let Some(v) = answer(label, prompt) {
                return v;
            }
            let base = label.trim_end_matches("!retry").trim_end_matches("!gate");
            if base == "read" {
                reading.clone()
            } else if base == "challenge" {
                challenge.clone()
            } else if let Some(n) = base.strip_prefix("review#") {
                review(n.parse().unwrap())
            } else if base.starts_with("work:") {
                json!({ "summary": "done" })
            } else if base.starts_with("fix:") {
                if innocent(label) {
                    json!({ "summary": "", "blocked": "not caused by my unit" })
                } else {
                    json!({ "summary": "patched it" })
                }
            } else if base == "learn" {
                learn_reply(prompt)
            } else {
                panic!("unexpected agent {label}")
            }
        }),
    };
    let last = Arc::new(Mutex::new(String::new()));
    let checks = FakeChecks {
        checker: o.checker.clone(),
        seen: Mutex::new(HashMap::new()),
        last: last.clone(),
        fail_to_start: o.checks_fail_to_start,
    };
    let sandbox: Box<dyn Sandbox> = match &o.sandbox {
        Some(spec) => Box::new(FakeSandbox {
            spec: spec.clone(),
            down: Mutex::new(spec.unavailable.map(String::from)),
            checker: o.checker.clone(),
            seen: Mutex::new(HashMap::new()),
            last: last.clone(),
        }),
        None => Box::new(wg::sandbox::client::NoSandbox(
            "no sandbox in this test".into(),
        )),
    };
    let repo_port = FakeRepo {
        tree: o.tree.clone(),
        last,
        changed: o.changed.clone(),
        diffs: o.diffs.clone(),
    };
    let hx =
        o.hx.clone()
            .unwrap_or_else(|| Arc::new(fake_hx(&o.units, &o.checks)));
    let memory_path = dirs.memory("o__r");
    if let Some(m) = &o.memory {
        std::fs::create_dir_all(memory_path.parent().unwrap()).unwrap();
        std::fs::write(&memory_path, m).unwrap();
    }
    let ctx = RunCtx {
        ports: Ports {
            proc: &proc,
            checks: &checks,
            repo: &repo_port,
            hx: &*hx,
            sandbox: &*sandbox,
        },
        cfg: &cfg,
        dirs: dirs.clone(),
        run_id: run_id.clone(),
        run_dir: run_dir.clone(),
        run_tmp: dirs.run_tmp(&run_id),
        repo: repo.clone(),
        repo_name: "o/r".into(),
        journal: Journal::open(&run_dir.join("journal.jsonl"), &run_id).unwrap(),
        ledger: Ledger::new(&dirs.ledger(), &run_id, cfg.max_calls_per_task),
        wg_exe: PathBuf::from("/opt/wg/bin/wg"),
        haruspex_cli: CLI.into(),
        plugins: vec![],
        user: "tester".into(),
        replay: wg::claude::replay::Replay::none(),
        shift: o.shift.then(|| {
            let steps = std::sync::atomic::AtomicUsize::new(0);
            let stop = o.stop;
            wg::engine::ShiftCtx {
                gh: wg::gh::Gh::new("/nonexistent/gh", base.join("gh")),
                issue: 1,
                branch: "wg/1-thing".into(),
                base: "main".into(),
                attribution: "Co-Authored-By: t".into(),
                shift_id: "20261120-0130".into(),
                max_parallel: o.max_parallel,
                stop: Box::new(move || {
                    let n = steps.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    stop.and_then(|(after, why)| (n >= after).then(|| why.to_string()))
                }),
            }
        }),
    };
    let dirty: Vec<String> = o.dirty.iter().map(|s| s.to_string()).collect();
    let decisions: Vec<Decision> = o
        .decisions
        .iter()
        .filter_map(|(id, a)| {
            a.map(|a| Decision {
                id: id.to_string(),
                answer: a.to_string(),
            })
        })
        .collect();
    let start = Start {
        title: o.task.trim().lines().next().unwrap_or_default().to_string(),
        task: o.task.trim().to_string(),
        decisions,
        resume_task: o.resume_task.map(String::from),
        initialized_earlier: o.initialized,
        user_changes_before: o
            .user_changes
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .unwrap_or(dirty),
        memory: wg::memory_store::load(&memory_path),
        memory_path: memory_path.clone(),
        pipeline: {
            let p = wg::pipelines::load(wg::assets::DEV_PIPELINE, 60).unwrap();
            wg_factory::api::PipelineDef {
                publish: if o.shift { p.publish.clone() } else { None },
                ..p
            }
        },
        start_branch: "main".into(),
        initializing: false,
        baseline_floor: o
            .baseline_floor
            .map(|v| v.iter().map(|s| s.to_string()).collect()),
        tainted_earlier: o.tainted_earlier,
    };
    let ran: Ran = driver::run_task(&ctx, start, step).expect("the run completes");
    wg::cli::run::persist(&ctx, &dirs, &ran).unwrap();
    let result = wg::report::output(&ctx, &ran).unwrap();
    let journal = wg::journal::read(&run_dir.join("journal.jsonl")).unwrap();
    let calls = journal
        .iter()
        .filter(|e| e["kind"] == "command" && e["label"].is_string())
        .map(|e| e["label"].as_str().unwrap().to_string())
        .collect();
    let saved = if result["memorySaved"] == true {
        Some(serde_json::from_str(&std::fs::read_to_string(&memory_path).unwrap()).unwrap())
    } else {
        None
    };
    Out {
        max_in_flight: proc.max_in_flight.load(std::sync::atomic::Ordering::SeqCst),
        result,
        calls,
        proc_calls: proc.calls.into_inner().unwrap(),
        saved,
        journal,
        run_dir,
        home,
    }
}

pub fn run(o: Opts) -> Out {
    run_with(o, &wg_factory::engine::step)
}

pub fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect()
}

pub fn exists_under(root: &Path, name_part: &str) -> bool {
    walk(root)
        .iter()
        .any(|p| p.to_string_lossy().contains(name_part))
}

pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            }
            out.push(p);
        }
    }
    out
}
