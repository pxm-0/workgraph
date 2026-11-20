//! One job on the sandbox (design §8.5 steps 4-7): pod, src volume, services, steps, the optional
//! patch, then cleanup that always runs (explicitly, and from a drop guard if anything unwinds)
//! and counts what is left by label.

use super::podman::{self, Podman};
use crate::sandbox::job::{tail, Cleanup, Job, JobResult, StepResult, MAX_PATCH};
use std::path::Path;
use std::time::{Duration, Instant};

/// Podman calls that are not steps (create, import, inspect, rm) get this long.
const ADMIN: Duration = Duration::from_secs(300);
const SNAPSHOT: &str = "git init -q . && git add -A && git -c user.email=wg@sandbox.invalid -c user.name=wg commit -q --no-verify -m base";
const DIFF: &str = "git add -A && git diff --cached --binary HEAD 2>/dev/null";

/// The volumes a job creates, so cleanup knows them even if creation failed half way.
pub fn volumes(job: &Job) -> Vec<String> {
    let mut v = vec![podman::src_volume(&job.job)];
    v.extend(
        job.services
            .iter()
            .filter(|s| s.data.is_some())
            .map(|s| podman::data_volume(&job.job, &s.name)),
    );
    v
}

/// Removes the pod (and so every container in it), the job's volumes and any stray labelled
/// container, then counts what is still there.
pub fn cleanup(pm: &dyn Podman, job: &str, vols: &[String]) -> Cleanup {
    // Each removal is best effort: what failed shows up in the leftover counts below.
    let _ = pm.run(&podman::pod_rm(job), None, ADMIN);
    let strays = pm.run(&podman::leftovers("containers", job), None, ADMIN);
    for id in strays.out.split_whitespace() {
        let _ = pm.run(&podman::rm(id), None, ADMIN);
    }
    let _ = pm.run(&podman::volume_rm(vols), None, ADMIN);
    let count = |kind: &str| {
        let r = pm.run(&podman::leftovers(kind, job), None, ADMIN);
        if r.ok() {
            r.out.split_whitespace().count() as u32
        } else {
            // A failed count is not a clean sandbox.
            u32::MAX
        }
    };
    Cleanup {
        containers_left: count("containers"),
        volumes_left: count("volumes"),
    }
}

/// Cleanup on unwind: an explicit `finish` disarms it.
struct Guard<'a> {
    pm: &'a dyn Podman,
    job: String,
    vols: Vec<String>,
    armed: bool,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if self.armed {
            cleanup(self.pm, &self.job, &self.vols);
        }
    }
}

fn must(r: podman::Ran, what: &str) -> Result<podman::Ran, String> {
    if r.ok() {
        Ok(r)
    } else {
        Err(format!(
            "{what} failed: {}",
            crate::ids::one_line(&tail(&r.out), 300)
        ))
    }
}

/// Runs one step container to completion, then reads its OOM flag and removes it.
fn step(
    pm: &dyn Podman,
    job: &Job,
    idx: usize,
    name: &str,
    cmd: &str,
    secs: u32,
    created: u64,
) -> (StepResult, String) {
    let cname = format!("wgsbx-{}-s{idx}", job.job);
    let mut args = podman::step(&job.job, &cname, &job.image, created, &job.limits);
    args.extend(podman::step_command(secs, cmd));
    let r = pm.run(&args, None, Duration::from_secs(u64::from(secs) + 60));
    let inspected = pm.run(&podman::inspect(&cname), None, ADMIN);
    let oom = serde_json::from_str::<serde_json::Value>(&inspected.out)
        .ok()
        .and_then(|v| v[0]["State"]["OOMKilled"].as_bool())
        .unwrap_or(false);
    let _ = pm.run(&podman::rm(&cname), None, ADMIN);
    let exit = if r.timed_out {
        124
    } else {
        i64::from(r.code.unwrap_or(-1))
    };
    let timed_out = r.timed_out || exit == 124 || (exit == 137 && !oom);
    let result = StepResult {
        name: name.to_string(),
        exit,
        timed_out,
        oom,
        tail: tail(&r.out),
    };
    (result, r.out)
}

/// Executes `job` with its work tar at `tar`. Never fails: sandbox problems are the result's `error`.
pub fn execute(pm: &dyn Podman, job: &Job, tar: &Path, created: u64) -> JobResult {
    let vols = volumes(job);
    let mut guard = Guard {
        pm,
        job: job.job.clone(),
        vols: vols.clone(),
        armed: true,
    };
    let mut steps = Vec::new();
    let started = Instant::now();
    let wall = u64::from(job.limits.wall_secs);
    let outcome: Result<Option<String>, String> = (|| {
        must(
            pm.run(&podman::pod_create(&job.job, created), None, ADMIN),
            "pod create",
        )?;
        for v in &vols {
            must(
                pm.run(&podman::volume_create(v, &job.job, created), None, ADMIN),
                "volume create",
            )?;
        }
        must(
            pm.run(&podman::volume_import(&vols[0]), Some(tar), ADMIN),
            "volume import",
        )?;
        for svc in &job.services {
            let mut a = podman::service(&job.job, svc, created, &job.limits);
            a.insert(1, "-d".into());
            must(pm.run(&a, None, ADMIN), &format!("service {}", svc.name))?;
        }
        if job.return_patch {
            let (r, _) = step(pm, job, 90, "snapshot", SNAPSHOT, 120, created);
            if r.exit != 0 {
                return Err(format!(
                    "the image cannot snapshot the work for a patch (is git in it?): {}",
                    crate::ids::one_line(&r.tail, 200)
                ));
            }
        }
        for (i, s) in job.steps.iter().enumerate() {
            let left = wall.saturating_sub(started.elapsed().as_secs());
            if left == 0 {
                return Err("the job's wall time ran out".into());
            }
            let secs = s.timeout_secs.min(left as u32);
            steps.push(step(pm, job, i, &s.name, &s.cmd, secs, created).0);
        }
        if !job.return_patch {
            return Ok(None);
        }
        let (r, out) = step(pm, job, 91, "diff", DIFF, 120, created);
        if r.exit != 0 {
            return Err(format!(
                "the patch could not be taken: {}",
                crate::ids::one_line(&r.tail, 200)
            ));
        }
        if out.len() > MAX_PATCH {
            return Err("the patch is over 2 MB".into());
        }
        Ok((!out.is_empty()).then(|| crate::sandbox::patch::encode(out.as_bytes())))
    })();
    guard.armed = false;
    let cleanup = cleanup(pm, &job.job, &vols);
    let (patch, error) = match outcome {
        Ok(p) => (p, None),
        Err(e) => (None, Some(e)),
    };
    JobResult {
        job: job.job.clone(),
        nonce: job.nonce.clone(),
        steps,
        patch,
        cleanup,
        error,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::sandbox::job::{Job, Service, Step, CEILING, PROTOCOL};
    use std::sync::Mutex;

    /// Records every call; answers by the first matching rule (substring of the joined argv).
    pub struct Fake {
        pub calls: Mutex<Vec<String>>,
        pub rules: Vec<(&'static str, Ran)>,
    }

    use super::podman::Ran;

    impl Podman for Fake {
        fn run(&self, args: &[String], _stdin: Option<&Path>, _t: Duration) -> Ran {
            let line = args.join(" ");
            self.calls.lock().unwrap().push(line.clone());
            self.rules
                .iter()
                .find(|(k, _)| line.contains(k))
                .map(|(_, r)| r.clone())
                .unwrap_or(Ran {
                    code: Some(0),
                    out: String::new(),
                    timed_out: false,
                })
        }
    }

    pub fn ran(code: i32, out: &str) -> Ran {
        Ran {
            code: Some(code),
            out: out.into(),
            timed_out: false,
        }
    }

    pub fn job(steps: &[(&str, &str)]) -> Job {
        Job {
            v: PROTOCOL,
            job: "j-0000000a".into(),
            nonce: "n-1".into(),
            image: "docker.io/library/busybox@sha256:".to_string() + &"a".repeat(64),
            prepare: None,
            services: vec![],
            steps: steps
                .iter()
                .map(|(n, c)| Step {
                    name: n.to_string(),
                    cmd: c.to_string(),
                    timeout_secs: 60,
                })
                .collect(),
            return_patch: false,
            limits: CEILING,
        }
    }

    #[test]
    fn steps_run_in_order_and_cleanup_always_runs() {
        let pm = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![
                ("sh -c fail", ran(3, "boom\n")),
                ("sh -c hog", ran(137, "Killed")),
                (
                    "inspect wgsbx-j-0000000a-s2",
                    ran(0, r#"[{"State":{"OOMKilled":true}}]"#),
                ),
                ("sh -c slow", ran(124, "")),
            ],
        };
        let mut j = job(&[
            ("ok", "true"),
            ("fail", "fail"),
            ("hog", "hog"),
            ("slow", "slow"),
        ]);
        j.services = vec![Service {
            name: "db".into(),
            image: j.image.clone(),
            env: Default::default(),
            data: Some("/data".into()),
        }];
        let r = execute(&pm, &j, Path::new("/x.tar"), 7);
        let got: Vec<(i64, bool, bool)> = r
            .steps
            .iter()
            .map(|s| (s.exit, s.timed_out, s.oom))
            .collect();
        assert_eq!(
            got,
            [
                (0, false, false),
                (3, false, false),
                (137, false, true),
                (124, true, false)
            ]
        );
        assert_eq!(r.steps[1].tail, "boom");
        assert!(r.error.is_none() && !r.passed());
        assert_eq!(r.cleanup, Cleanup::default());
        let calls = pm.calls.lock().unwrap().clone();
        assert!(calls[0].starts_with("pod create --name wgsbx-j-0000000a --network none"));
        assert!(calls
            .iter()
            .any(|c| c.starts_with("volume create") && c.ends_with("wgsbx-j-0000000a-svc-db")));
        assert!(calls
            .iter()
            .any(|c| c.starts_with("run -d --name wgsbx-j-0000000a-db")));
        assert!(calls
            .iter()
            .any(|c| c == "pod rm -f --time 5 wgsbx-j-0000000a"));
        assert!(calls
            .iter()
            .any(|c| c == "volume rm -f wgsbx-j-0000000a-src wgsbx-j-0000000a-svc-db"));
    }

    #[test]
    fn a_failed_setup_is_an_error_and_still_cleans_up() {
        let pm = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![
                ("volume import", ran(125, "no space left")),
                ("ps -a", ran(0, "abc\n")),
            ],
        };
        let r = execute(&pm, &job(&[("t", "true")]), Path::new("/x.tar"), 7);
        assert!(r
            .error
            .unwrap()
            .contains("volume import failed: no space left"));
        assert!(r.steps.is_empty());
        assert_eq!(r.cleanup.containers_left, 1, "a leftover is counted");
        let calls = pm.calls.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c == "rm -f --time 5 abc"));
        let failed_count = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![("volume ls", ran(1, ""))],
        };
        assert_eq!(cleanup(&failed_count, "j-1", &[]).volumes_left, u32::MAX);
    }

    #[test]
    fn a_patch_is_snapshotted_diffed_and_encoded() {
        let pm = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![("git diff --cached", ran(0, "diff --git a/m.sql b/m.sql\n"))],
        };
        let mut j = job(&[("gen", "make migration")]);
        j.return_patch = true;
        let r = execute(&pm, &j, Path::new("/x.tar"), 7);
        let p = crate::sandbox::patch::decode(r.patch.as_deref().unwrap()).unwrap();
        assert_eq!(p, b"diff --git a/m.sql b/m.sql\n");
        let calls = pm.calls.lock().unwrap().clone();
        let snap = calls
            .iter()
            .position(|c| c.contains("git init -q"))
            .unwrap();
        let gen = calls
            .iter()
            .position(|c| c.contains("make migration"))
            .unwrap();
        assert!(snap < gen);
        let no_git = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![("git init", ran(127, "sh: git: not found"))],
        };
        assert!(execute(&no_git, &j, Path::new("/x.tar"), 7)
            .error
            .unwrap()
            .contains("git"));
    }
}
