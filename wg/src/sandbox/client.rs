//! The Mac side of a job (design §8.5 steps 1-2, 7): build it, stream it through
//! `orb run -m <machine> -u <user> /usr/local/lib/wgsbx/wg sandbox-agent <sub>`, poll it, and
//! check the answer against what was sent. No task data is ever placed in argv or a shell string.

use super::config::{Machine, RepoSandbox};
use super::job::{self, Job, JobResult, Polled, Step, CEILING, PROTOCOL};
use crate::config::Config;
use crate::ports::Sandbox;
use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A single `wg sandbox exec` call returns within this, well under Claude's 10-minute Bash limit.
pub const EXEC_CAP: Duration = Duration::from_secs(8 * 60);
const CALL: Duration = Duration::from_secs(300);
const POLL_EVERY: Duration = Duration::from_secs(2);

pub struct Client {
    pub orb: String,
    pub machine: Machine,
}

/// Runs `program` with `stdin` bytes, its own process group and a timeout. `(exit, stdout, stderr)`.
fn pipe(
    program: &str,
    args: &[String],
    stdin: &[u8],
    timeout: Duration,
) -> Result<(i32, String, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(crate::exec::env::scrubbed())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("sandbox unavailable: {program} failed to start: {e}"))?;
    let mut w = child.stdin.take().expect("piped");
    let input = stdin.to_vec();
    // A transport that exits early closes the pipe; its exit code says why.
    let writer = std::thread::spawn(move || {
        let _ = w.write_all(&input);
    });
    let grab = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                let _ = r.take(job::MAX_RESULT as u64 * 4).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = grab(
        child
            .stdout
            .take()
            .map(|x| Box::new(x) as Box<dyn Read + Send>),
    );
    let err = grab(
        child
            .stderr
            .take()
            .map(|x| Box::new(x) as Box<dyn Read + Send>),
    );
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) if start.elapsed() >= timeout => {
                crate::exec::proc::kill_group(child.id());
                let _ = child.wait();
                return Err(format!(
                    "sandbox unavailable: {program} timed out after {}s",
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("sandbox unavailable: {e}")),
        }
    };
    let _ = writer.join();
    let text = |h: std::thread::JoinHandle<Vec<u8>>| {
        String::from_utf8_lossy(&h.join().unwrap_or_default()).into_owned()
    };
    Ok((code, text(out), text(err)))
}

impl Client {
    pub fn new(cfg: &Config) -> Client {
        Client {
            orb: cfg.orb_bin.clone(),
            machine: cfg.sandbox.clone(),
        }
    }

    /// The compile-time shape: only the subcommand and a validated job id vary.
    pub fn argv(&self, sub: &str, job: Option<&str>) -> Vec<String> {
        let mut a: Vec<String> = [
            "run",
            "-m",
            &self.machine.machine,
            "-u",
            &self.machine.user,
            super::AGENT,
            "sandbox-agent",
            sub,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(j) = job {
            a.extend(["--job".to_string(), j.to_string()]);
        }
        a
    }

    /// One agent call; its last stdout line is the answer, an `{"error":...}` line a refusal.
    pub fn call(
        &self,
        sub: &str,
        job: Option<&str>,
        stdin: &[u8],
        timeout: Duration,
    ) -> Result<String, String> {
        if let Some(j) = job {
            job::job_id(j)?;
        }
        let (code, out, err) = pipe(&self.orb, &self.argv(sub, job), stdin, timeout)?;
        let line = out
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string();
        // A job's own result may carry `error` too; only an answer about no job is a refusal.
        let refusal = serde_json::from_str::<Value>(&line)
            .ok()
            .filter(|v| v.get("job").is_none())
            .and_then(|v| v["error"].as_str().map(str::to_string));
        match (code, refusal) {
            (0, None) if !line.is_empty() => Ok(line),
            (_, Some(e)) => Err(format!(
                "sandbox unavailable: agent: {}",
                crate::ids::one_line(&e, 300)
            )),
            (c, None) => Err(format!(
                "sandbox unavailable: the agent call exited {c}: {}",
                crate::ids::one_line(&crate::exec::run::tail(&format!("{out}{err}")), 300)
            )),
        }
    }

    pub fn submit(&self, j: &Job, root: &Path) -> Result<(), String> {
        let stream = super::bundle::stream(j, root)?;
        let line = self.call("submit", None, &stream, CALL)?;
        let v: Value = serde_json::from_str(&line)
            .map_err(|_| "sandbox unavailable: submit answered no JSON".to_string())?;
        if v["job"].as_str() != Some(j.job.as_str()) || v["submitted"] != true {
            return Err("sandbox unavailable: submit did not accept the job".into());
        }
        Ok(())
    }

    pub fn poll(&self, j: &Job) -> Result<Polled, String> {
        job::check_result(&self.call("poll", Some(&j.job), &[], CALL)?, j)
    }

    /// Polls until the job is done or `cap` passes (then `Pending` or `Queued`).
    pub fn wait(&self, j: &Job, cap: Duration) -> Result<Polled, String> {
        self.wait_running(j, cap, Duration::ZERO)
    }

    /// Like `wait`, but time the job spends queued behind another job (the agent runs one at a
    /// time) counts against `queue`, not `cap`: `cap` starts once the job runs.
    pub fn wait_running(&self, j: &Job, cap: Duration, queue: Duration) -> Result<Polled, String> {
        let start = Instant::now();
        let mut running: Option<Instant> = None;
        loop {
            let p = self.poll(j)?;
            if matches!(p, Polled::Done(_)) {
                return Ok(p);
            }
            if p == Polled::Pending || start.elapsed() >= queue {
                running.get_or_insert_with(Instant::now);
            }
            if running.is_some_and(|t| t.elapsed() + POLL_EVERY >= cap) {
                return Ok(p);
            }
            std::thread::sleep(POLL_EVERY);
        }
    }

    /// Stops a job the Mac gave up on; its unit's cleanup still runs.
    pub fn cancel(&self, j: &Job) -> Result<(), String> {
        self.call("cancel", Some(&j.job), &[], CALL).map(|_| ())
    }

    pub fn doctor(&self) -> Result<Value, String> {
        let line = self.call("doctor", None, &[], CALL)?;
        serde_json::from_str(&line)
            .map_err(|_| "sandbox unavailable: the doctor answered no JSON".into())
    }

    pub fn reap(&self, older_secs: u64) -> Result<Value, String> {
        let mut a = self.argv("reap", None);
        a.extend(["--older-than".to_string(), older_secs.to_string()]);
        let (code, out, err) = pipe(&self.orb, &a, &[], CALL)?;
        let line = out.lines().last().unwrap_or("");
        match serde_json::from_str::<Value>(line) {
            Ok(v) if code == 0 => Ok(v),
            _ => Err(format!(
                "sandbox reap failed: {}",
                crate::ids::one_line(&format!("{line}{err}"), 300)
            )),
        }
    }
}

/// A job for `repo` with `steps`, at the agent's own ceilings.
pub fn new_job(
    sb: &RepoSandbox,
    steps: Vec<Step>,
    nonce: &str,
    return_patch: bool,
) -> Result<Job, String> {
    let j = Job {
        v: PROTOCOL,
        job: format!("j-{}", crate::tokens_store::random_hex(8)?),
        nonce: crate::ids::one_line(nonce, 64).replace(
            |c: char| !(c.is_ascii_alphanumeric() || ":_#.-".contains(c)),
            "_",
        ),
        image: sb.image.clone(),
        prepare: None,
        services: sb.services(),
        steps,
        return_patch,
        limits: CEILING,
    };
    job::validate(&j, &|_| true)?;
    Ok(j)
}

/// Whether risky work may run: unchecked, or the doctor's verdict, shared by every task of a shift.
pub type Status = Arc<Mutex<Option<Result<(), String>>>>;

/// A job waits at most this long behind others before the Mac gives up on it.
pub const QUEUE: Duration = Duration::from_secs(60 * 60);

/// Why the sandbox may not be trusted with more work, from the jobs a run's agents sent through
/// `wg sandbox exec` (`<run>/sandbox.jsonl`): leftovers, or a job lost without a result.
pub fn recorded_fault(run_dir: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(run_dir.join("sandbox.jsonl")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read the run's sandbox jobs: {e}")),
    };
    for line in text.lines() {
        let v: Value = serde_json::from_str(line)
            .map_err(|e| format!("the run's sandbox jobs are corrupt: {e}"))?;
        let job = v["job"].as_str().unwrap_or("?");
        let left = |k: &str| v["cleanup"][k].as_u64().unwrap_or(0);
        if left("containers_left") > 0 || left("volumes_left") > 0 {
            return Ok(Some(format!(
                "sandbox dirty: job {job} left {} containers and {} volumes",
                left("containers_left"),
                left("volumes_left")
            )));
        }
        if let Some(e) = v["error"].as_str().filter(|e| lost(e)) {
            return Ok(Some(format!("job {job}: {e}")));
        }
    }
    Ok(None)
}

/// The agent lost the job (it was stopped, or the job is gone): not the code's fault.
fn lost(error: &str) -> bool {
    error.contains("without a result") || error.contains("gone")
}

/// The engine's sandbox port for one repo and worktree.
pub struct SimSandbox {
    pub cfg: Config,
    pub repo: Option<RepoSandbox>,
    pub root: PathBuf,
    pub status: Status,
}

impl SimSandbox {
    fn broken(&self, why: &str) {
        if let Ok(mut s) = self.status.lock() {
            *s = Some(Err(why.to_string()));
        }
    }
}

impl Sandbox for SimSandbox {
    fn unavailable(&self) -> Option<String> {
        if self.repo.is_none() {
            return Some("this repo has no [repo.sandbox]".into());
        }
        let mut s = match self.status.lock() {
            Ok(s) => s,
            Err(_) => return Some("the sandbox status is poisoned".into()),
        };
        if s.is_none() {
            let d = super::doctor::doctor(&self.cfg);
            *s = Some(match d["failed"].as_str() {
                None if d["ok"] == true => Ok(()),
                other => Err(other.unwrap_or("the doctor failed").to_string()),
            });
        }
        s.clone().and_then(Result::err)
    }

    fn mark_broken(&self, why: &str) {
        self.broken(why);
    }

    fn run(&self, nonce: &str, steps: Vec<Step>) -> Result<JobResult, String> {
        if let Some(why) = self.unavailable() {
            return Err(format!("sandbox unavailable: {why}"));
        }
        let repo = self.repo.as_ref().expect("checked by unavailable");
        let client = Client::new(&self.cfg);
        let j = new_job(repo, steps, nonce, false)?;
        let wall = Duration::from_secs(u64::from(j.limits.wall_secs) + 120);
        let polled = client
            .submit(&j, &self.root)
            .and_then(|_| client.wait_running(&j, wall, QUEUE));
        match polled {
            Ok(Polled::Done(r)) => {
                if r.cleanup.containers_left > 0 || r.cleanup.volumes_left > 0 {
                    self.broken(&format!(
                        "sandbox dirty: job {} left {} containers and {} volumes",
                        r.job, r.cleanup.containers_left, r.cleanup.volumes_left
                    ));
                }
                match &r.error {
                    Some(e) => {
                        if lost(e) {
                            self.broken(e);
                        }
                        Err(format!("sandbox unavailable: {e}"))
                    }
                    None => Ok(r),
                }
            }
            Ok(p) => {
                let why = match p {
                    Polled::Queued => "a job waited over an hour behind others",
                    _ => "a job outlived its wall time",
                };
                let cancelled = client
                    .cancel(&j)
                    .err()
                    .map_or(String::new(), |e| format!(" (cancel failed: {e})"));
                self.broken(&format!("{why}{cancelled}"));
                Err(format!(
                    "sandbox unavailable: job {} did not finish: {why}{cancelled}",
                    j.job
                ))
            }
            Err(e) => {
                self.broken(&e);
                Err(e)
            }
        }
    }
}

/// The port when no sandbox can be used (no `[repo.sandbox]`, or the caller decided).
pub struct NoSandbox(pub String);

impl Sandbox for NoSandbox {
    fn unavailable(&self) -> Option<String> {
        Some(self.0.clone())
    }
    fn run(&self, _nonce: &str, _steps: Vec<Step>) -> Result<JobResult, String> {
        Err(format!("sandbox unavailable: {}", self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_argv_is_fixed_and_carries_no_task_data() {
        let c = Client {
            orb: "/usr/local/bin/orb".into(),
            machine: Machine {
                machine: "oreochiserver-sim".into(),
                user: "wgsbx".into(),
                probe: String::new(),
            },
        };
        assert_eq!(
            c.argv("poll", Some("j-0000000a")),
            [
                "run",
                "-m",
                "oreochiserver-sim",
                "-u",
                "wgsbx",
                "/usr/local/lib/wgsbx/wg",
                "sandbox-agent",
                "poll",
                "--job",
                "j-0000000a"
            ]
        );
        assert!(c
            .call("poll", Some("../x"), &[], CALL)
            .unwrap_err()
            .contains("job id"));
        let missing = Client {
            orb: "/nonexistent/orb".into(),
            ..c
        };
        assert!(missing
            .call("doctor", None, &[], CALL)
            .unwrap_err()
            .starts_with("sandbox unavailable"));
    }

    #[test]
    fn jobs_get_a_fresh_id_and_a_safe_nonce() {
        let sb: RepoSandbox = toml::from_str(&format!(
            "image = \"docker.io/library/node@sha256:{}\"",
            "a".repeat(64)
        ))
        .unwrap();
        let steps = vec![Step {
            name: "test".into(),
            cmd: "npm test".into(),
            timeout_secs: 600,
        }];
        let a = new_job(&sb, steps.clone(), "r-1:check L1", false).unwrap();
        let b = new_job(&sb, steps, "r-1", false).unwrap();
        assert_ne!(a.job, b.job);
        assert_eq!(a.nonce, "r-1:check_L1");
        assert_eq!(a.limits, CEILING);
        assert!(NoSandbox("why".into())
            .run("n", vec![])
            .unwrap_err()
            .contains("why"));
    }

    #[test]
    fn a_job_still_running_at_the_cap_comes_back_pending_not_blocking() {
        let d = std::env::temp_dir().join(format!(
            "wg-client-cap-{}-{}",
            std::process::id(),
            crate::journal::now_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let orb = d.join("orb");
        std::fs::write(&orb, "#!/bin/sh\nfor a; do last=$a; done\necho \"{\\\"job\\\":\\\"$last\\\",\\\"pending\\\":true}\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&orb, std::fs::Permissions::from_mode(0o755)).unwrap();
        let c = Client {
            orb: orb.to_string_lossy().into(),
            machine: Machine {
                machine: "m".into(),
                user: "u".into(),
                probe: String::new(),
            },
        };
        let sb: RepoSandbox = toml::from_str(&format!(
            "image = \"docker.io/library/node@sha256:{}\"",
            "a".repeat(64)
        ))
        .unwrap();
        let j = new_job(
            &sb,
            vec![Step {
                name: "t".into(),
                cmd: "sleep 600".into(),
                timeout_secs: 900,
            }],
            "n",
            false,
        )
        .unwrap();
        let t = Instant::now();
        assert_eq!(c.wait(&j, Duration::from_secs(5)).unwrap(), Polled::Pending);
        assert!(t.elapsed() < Duration::from_secs(6), "never past the cap");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn time_queued_behind_another_job_does_not_count_against_the_wall() {
        let d = std::env::temp_dir().join(format!(
            "wg-client-queue-{}-{}",
            std::process::id(),
            crate::journal::now_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        // Answers queued while `<d>/queued` exists (removing it once), then running.
        let script = r#"#!/bin/sh
for a; do last=$a; done
if [ -e DIR/always ] || [ -e DIR/queued ]; then rm -f DIR/queued; echo "{\"job\":\"$last\",\"pending\":true,\"queued\":true}"; else echo "{\"job\":\"$last\",\"pending\":true}"; fi
"#;
        let orb = d.join("orb");
        std::fs::write(&orb, script.replace("DIR", &d.to_string_lossy())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&orb, std::fs::Permissions::from_mode(0o755)).unwrap();
        let c = Client {
            orb: orb.to_string_lossy().into(),
            machine: Machine {
                machine: "m".into(),
                user: "u".into(),
                probe: String::new(),
            },
        };
        let sb: RepoSandbox = toml::from_str(&format!(
            "image = \"docker.io/library/node@sha256:{}\"",
            "a".repeat(64)
        ))
        .unwrap();
        let step = Step {
            name: "t".into(),
            cmd: "true".into(),
            timeout_secs: 5,
        };
        let j = new_job(&sb, vec![step], "n", false).unwrap();
        std::fs::write(d.join("queued"), "").unwrap();
        let t = Instant::now();
        let got = c
            .wait_running(&j, Duration::from_secs(3), Duration::from_secs(60))
            .unwrap();
        assert_eq!(got, Polled::Pending);
        assert!(
            t.elapsed() >= Duration::from_secs(4),
            "the cap started after the queue"
        );
        std::fs::write(d.join("always"), "").unwrap();
        let t = Instant::now();
        let got = c
            .wait_running(&j, Duration::from_secs(1), Duration::from_secs(1))
            .unwrap();
        assert_eq!(got, Polled::Queued, "the queue has its own cap");
        assert!(t.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn exec_faults_are_read_back_from_the_run() {
        let d = std::env::temp_dir().join(format!(
            "wg-client-fault-{}-{}",
            std::process::id(),
            crate::journal::now_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(recorded_fault(&d).unwrap(), None, "no jobs yet");
        let line = |v: &str| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(d.join("sandbox.jsonl"))
                .unwrap();
            writeln!(f, "{v}").unwrap();
        };
        line(r#"{"job":"j-1","cleanup":{"containers_left":0,"volumes_left":0},"error":null}"#);
        line(
            r#"{"job":"j-2","cleanup":{"containers_left":0,"volumes_left":0},"error":"the job's wall time ran out"}"#,
        );
        assert_eq!(
            recorded_fault(&d).unwrap(),
            None,
            "a code failure is no fault"
        );
        line(
            r#"{"job":"j-3","cleanup":{"containers_left":0,"volumes_left":0},"error":"the sandbox job is gone without a result"}"#,
        );
        assert!(recorded_fault(&d).unwrap().unwrap().contains("j-3"));
        std::fs::write(
            d.join("sandbox.jsonl"),
            r#"{"job":"j-4","cleanup":{"containers_left":0,"volumes_left":2},"error":null}"#,
        )
        .unwrap();
        assert!(recorded_fault(&d)
            .unwrap()
            .unwrap()
            .starts_with("sandbox dirty: job j-4"));
        std::fs::write(d.join("sandbox.jsonl"), "not json").unwrap();
        assert!(recorded_fault(&d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_job_result_carrying_an_error_is_a_result_not_a_refusal() {
        let d = std::env::temp_dir().join(format!(
            "wg-client-err-{}-{}",
            std::process::id(),
            crate::journal::now_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let orb = d.join("orb");
        std::fs::write(&orb, "#!/bin/sh\nfor a; do last=$a; done\necho \"{\\\"job\\\":\\\"$last\\\",\\\"nonce\\\":\\\"n\\\",\\\"steps\\\":[],\\\"cleanup\\\":{\\\"containers_left\\\":0,\\\"volumes_left\\\":0},\\\"error\\\":\\\"the job's wall time ran out\\\"}\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&orb, std::fs::Permissions::from_mode(0o755)).unwrap();
        let c = Client {
            orb: orb.to_string_lossy().into(),
            machine: Machine {
                machine: "m".into(),
                user: "u".into(),
                probe: String::new(),
            },
        };
        let sb: RepoSandbox = toml::from_str(&format!(
            "image = \"docker.io/library/node@sha256:{}\"",
            "a".repeat(64)
        ))
        .unwrap();
        let step = Step {
            name: "t".into(),
            cmd: "true".into(),
            timeout_secs: 5,
        };
        // The agent's stored result for a job that stopped early, as `poll` answers it.
        let j = new_job(&sb, vec![step], "n", false).unwrap();
        match c.poll(&j).unwrap() {
            Polled::Done(r) => assert_eq!(r.error.as_deref(), Some("the job's wall time ran out")),
            p => panic!("{p:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_fake_transport_round_trips_a_job() {
        let d = std::env::temp_dir().join(format!(
            "wg-client-{}-{}",
            std::process::id(),
            crate::journal::now_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        // The fake orb answers submit with the job id it read, and poll with a passing result.
        let orb = d.join("orb");
        std::fs::write(&orb, "#!/bin/sh\nfor a; do last=$a; prev=$p; p=$a; done\ncase \"$8\" in\nsubmit) head -n 1 | sed 's/.*\"job\":\"\\(j-[0-9a-f]*\\)\".*/{\"job\":\"\\1\",\"submitted\":true}/' ;;\npoll) echo \"{\\\"job\\\":\\\"$last\\\",\\\"nonce\\\":\\\"n\\\",\\\"steps\\\":[{\\\"name\\\":\\\"t\\\",\\\"exit\\\":0,\\\"timed_out\\\":false,\\\"oom\\\":false,\\\"tail\\\":\\\"ok\\\"}],\\\"cleanup\\\":{\\\"containers_left\\\":0,\\\"volumes_left\\\":0}}\" ;;\n*) echo '{\"error\":\"no\"}'; exit 2 ;;\nesac\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&orb, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&d)
            .status()
            .unwrap()
            .success());
        let c = Client {
            orb: orb.to_string_lossy().into(),
            machine: Machine {
                machine: "m".into(),
                user: "u".into(),
                probe: String::new(),
            },
        };
        let sb: RepoSandbox = toml::from_str(&format!(
            "image = \"docker.io/library/node@sha256:{}\"",
            "a".repeat(64)
        ))
        .unwrap();
        let j = new_job(
            &sb,
            vec![Step {
                name: "t".into(),
                cmd: "true".into(),
                timeout_secs: 5,
            }],
            "n",
            false,
        )
        .unwrap();
        c.submit(&j, &d).unwrap();
        assert!(
            matches!(c.wait(&j, Duration::from_secs(30)).unwrap(), Polled::Done(r) if r.passed())
        );
        assert!(c.doctor().unwrap_err().contains("agent: no"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
