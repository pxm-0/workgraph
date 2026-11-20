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
        let refusal = serde_json::from_str::<Value>(&line)
            .ok()
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

    /// Polls until the job is done or `cap` passes (then `Pending`).
    pub fn wait(&self, j: &Job, cap: Duration) -> Result<Polled, String> {
        let start = Instant::now();
        loop {
            if let Polled::Done(r) = self.poll(j)? {
                return Ok(Polled::Done(r));
            }
            if start.elapsed() + POLL_EVERY >= cap {
                return Ok(Polled::Pending);
            }
            std::thread::sleep(POLL_EVERY);
        }
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
            .and_then(|_| client.wait(&j, wall));
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
                        if e.contains("without a result") || e.contains("gone") {
                            self.broken(e);
                        }
                        Err(format!("sandbox unavailable: {e}"))
                    }
                    None => Ok(r),
                }
            }
            Ok(Polled::Pending) => {
                self.broken("a job outlived its wall time");
                Err(format!("sandbox unavailable: job {} did not finish", j.job))
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
