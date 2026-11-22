//! Podman argv from typed fields only (design §8.3, §8.5 step 4). No builder here can emit a bind
//! mount, `--privileged` or a network: the work arrives in a named volume, every container joins
//! a pod made with `--network none`, and the doctor's probe uses these same builders.

use super::super::job::{Limits, Service};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Capabilities a service (e.g. postgres) needs to set up its data dir and drop to its own user.
pub const SERVICE_CAPS: &str = "CHOWN,SETUID,SETGID,FOWNER,DAC_OVERRIDE";
/// Where the work is mounted in every step container.
pub const WORK: &str = "/work";

/// One finished podman call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ran {
    /// `None` when podman never started or was killed for overrunning.
    pub code: Option<i32>,
    /// stdout then stderr, capped by the runner.
    pub out: String,
    pub timed_out: bool,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// The podman port: the real runner below, a scripted fake in tests.
pub trait Podman {
    fn run(&self, args: &[String], stdin: Option<&Path>, timeout: Duration) -> Ran;
}

/// The pod, volume and container names of one job: all prefixed `wgsbx-<job>`.
pub fn pod(job: &str) -> String {
    format!("wgsbx-{job}")
}
pub fn src_volume(job: &str) -> String {
    format!("wgsbx-{job}-src")
}
pub fn data_volume(job: &str, svc: &str) -> String {
    format!("wgsbx-{job}-svc-{svc}")
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn labels(job: &str, created: u64) -> Vec<String> {
    vec![
        "--label".into(),
        format!("wg.job={job}"),
        "--label".into(),
        format!("wg.created={created}"),
    ]
}

pub fn pod_create(job: &str, created: u64) -> Vec<String> {
    let mut a = s(&["pod", "create", "--name", &pod(job), "--network", "none"]);
    a.extend(labels(job, created));
    a
}

pub fn volume_create(name: &str, job: &str, created: u64) -> Vec<String> {
    let mut a = s(&["volume", "create"]);
    a.extend(labels(job, created));
    a.push(name.into());
    a
}

/// `podman volume import <vol> -`, the tar on stdin.
pub fn volume_import(name: &str) -> Vec<String> {
    s(&["volume", "import", name, "-"])
}

/// What every container of a job shares: the pod (so no network), labels, limits, no privileges.
fn common(job: &str, name: &str, created: u64, l: &Limits) -> Vec<String> {
    let mut a = s(&["run", "--name", name, "--pod", &pod(job)]);
    a.extend(labels(job, created));
    let mem = format!("{}m", l.memory_mb);
    a.extend(s(&[
        "--security-opt",
        "no-new-privileges",
        "--memory",
        &mem,
        "--memory-swap",
        &mem,
        "--cpus",
        &l.cpus.to_string(),
        "--pids-limit",
        &l.pids.to_string(),
        "--ulimit",
        "nofile=4096:4096",
        "--ulimit",
        "core=0",
        "--env-file",
        "/dev/null",
        "--read-only",
        // Bounded file logs: journald would keep a second copy of every step's output.
        "--log-driver",
        "k8s-file",
        "--log-opt",
        "max-size=1m",
    ]));
    a
}

/// The step profile up to and including the image; the caller appends the command.
pub fn step(job: &str, name: &str, image: &str, created: u64, l: &Limits) -> Vec<String> {
    let mut a = common(job, name, created, l);
    let tmp = format!("/tmp:rw,nosuid,nodev,size={}m", l.tmp_mb);
    let work = format!(
        "type=volume,source={},target={WORK},U=true",
        src_volume(job)
    );
    a.extend(s(&[
        "--tmpfs",
        &tmp,
        "--cap-drop",
        "ALL",
        "--user",
        "1000:1000",
        "--env",
        "HOME=/tmp",
        "--entrypoint",
        "",
        "--workdir",
        WORK,
        "--mount",
        &work,
        image,
    ]));
    a
}

/// A step's command: `timeout -k 10 <secs> sh -c <cmd>`, `<cmd>` one argv element.
pub fn step_command(secs: u32, cmd: &str) -> Vec<String> {
    s(&["sh", "-c", STEP, "wg-step", &secs.to_string(), cmd])
}

/// Marks a step whose container saw an OOM kill (podman reports `OOMKilled` only when the
/// container's first process dies, not when the kernel kills a child of it).
pub const OOM_MARK: &str = "[wg: oom_kill]";

/// The fixed step wrapper: `$1` is the timeout, `$2` the command, passed as arguments, never
/// spliced into this text.
const STEP: &str = r#"timeout -k 10 "$1" sh -c "$2"; rc=$?; if grep -qs '^oom_kill [1-9]' /sys/fs/cgroup/memory.events; then echo '[wg: oom_kill]' >&2; fi; exit $rc"#;

/// The service profile up to and including the image (design §8.3).
pub fn service(job: &str, svc: &Service, created: u64, l: &Limits) -> Vec<String> {
    let mut a = common(job, &format!("wgsbx-{job}-{}", svc.name), created, l);
    let tmp = format!("/tmp:rw,nosuid,nodev,size={}m", l.tmp_mb);
    a.extend(s(&[
        "--tmpfs",
        &tmp,
        "--cap-drop",
        "ALL",
        "--cap-add",
        SERVICE_CAPS,
    ]));
    if let Some(d) = &svc.data {
        a.push("--mount".into());
        a.push(format!(
            "type=volume,source={},target={d}",
            data_volume(job, &svc.name)
        ));
    }
    for (k, v) in &svc.env {
        a.push("--env".into());
        a.push(format!("{k}={v}"));
    }
    a.push(svc.image.clone());
    a
}

pub fn inspect(name: &str) -> Vec<String> {
    s(&["inspect", name, "--format", "json"])
}

/// `--volumes` takes the anonymous volumes an image's `VOLUME` made; `pod rm` has no such flag.
pub fn rm(name: &str) -> Vec<String> {
    s(&["rm", "-f", "--volumes", "--time", "5", name])
}

pub fn pod_rm(job: &str) -> Vec<String> {
    s(&["pod", "rm", "-f", "--time", "5", &pod(job)])
}

pub fn volume_rm(names: &[String]) -> Vec<String> {
    let mut a = s(&["volume", "rm", "-f"]);
    a.extend(names.iter().cloned());
    a
}

/// A job's containers as `<id> <is-infra>` lines: the pod's infra container can only go with
/// the pod, so cleanup must not `rm` it.
pub fn job_containers(job: &str) -> Vec<String> {
    let filter = format!("label=wg.job={job}");
    s(&[
        "ps",
        "-a",
        "--filter",
        &filter,
        "--format",
        "{{.ID}} {{.IsInfra}}",
    ])
}

/// Ids of a job's leftover containers (`ps`) or volumes (`volume ls`), always label-filtered.
pub fn leftovers(kind: &str, job: &str) -> Vec<String> {
    let filter = format!("label=wg.job={job}");
    match kind {
        "volumes" => s(&["volume", "ls", "--filter", &filter, "--format", "{{.Name}}"]),
        _ => s(&["ps", "-a", "--filter", &filter, "--format", "{{.ID}}"]),
    }
}

/// The environment podman runs with: nothing inherited (no Mac `PATH` from `orb run`).
pub fn env() -> Vec<(String, String)> {
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        })
        .unwrap_or_else(|| "0".into());
    let run = format!("/run/user/{uid}");
    vec![
        ("PATH".into(), "/usr/sbin:/usr/bin:/sbin:/bin".into()),
        ("HOME".into(), std::env::var("HOME").unwrap_or_default()),
        ("LANG".into(), "C.UTF-8".into()),
        (
            "DBUS_SESSION_BUS_ADDRESS".into(),
            format!("unix:path={run}/bus"),
        ),
        ("XDG_RUNTIME_DIR".into(), run),
    ]
}

/// Cap on captured output: a patch is at most 2 MB, so 3 MB leaves room to detect overflow.
const MAX_OUT: u64 = 3 * 1024 * 1024;

pub struct RealPodman;

impl Podman for RealPodman {
    fn run(&self, args: &[String], stdin: Option<&Path>, timeout: Duration) -> Ran {
        let input = match stdin.map(std::fs::File::open).transpose() {
            Ok(f) => f.map(Stdio::from).unwrap_or_else(Stdio::null),
            Err(e) => {
                return Ran {
                    code: None,
                    out: format!("cannot open the job input: {e}"),
                    timed_out: false,
                }
            }
        };
        let child = Command::new("podman")
            .args(args)
            .env_clear()
            .envs(env())
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                return Ran {
                    code: None,
                    out: format!("podman failed to start: {e}"),
                    timed_out: false,
                }
            }
        };
        let grab = |r: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(r) = r {
                    // Capture stops at the cap; podman's own exit code still decides.
                    let _ = r.take(MAX_OUT).read_to_end(&mut buf);
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
        let (code, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(st)) => break (st.code(), false),
                Ok(None) if start.elapsed() >= timeout => {
                    crate::exec::proc::kill_group(child.id());
                    let _ = child.wait();
                    break (None, true);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break (None, false),
            }
        };
        let text = |h: std::thread::JoinHandle<Vec<u8>>| {
            String::from_utf8_lossy(&h.join().unwrap_or_default()).into_owned()
        };
        Ran {
            code,
            out: format!("{}{}", text(out), text(err)),
            timed_out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::job::CEILING;

    fn no_host_reach(a: &[String]) {
        for bad in ["-v", "--volume", "--privileged", "--network=host", "--net"] {
            assert!(!a.iter().any(|x| x == bad), "{bad} in {a:?}");
        }
        assert!(
            !a.iter()
                .any(|x| x.contains("type=bind") || x.contains("/Users")),
            "{a:?}"
        );
    }

    #[test]
    fn the_step_profile_is_exact() {
        let a = step(
            "j-0000000a",
            "wgsbx-j-0000000a-s0",
            "img@sha256:x",
            7,
            &CEILING,
        );
        let want = [
            "run",
            "--name",
            "wgsbx-j-0000000a-s0",
            "--pod",
            "wgsbx-j-0000000a",
            "--label",
            "wg.job=j-0000000a",
            "--label",
            "wg.created=7",
            "--security-opt",
            "no-new-privileges",
            "--memory",
            "2048m",
            "--memory-swap",
            "2048m",
            "--cpus",
            "2",
            "--pids-limit",
            "512",
            "--ulimit",
            "nofile=4096:4096",
            "--ulimit",
            "core=0",
            "--env-file",
            "/dev/null",
            "--read-only",
            "--log-driver",
            "k8s-file",
            "--log-opt",
            "max-size=1m",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=512m",
            "--cap-drop",
            "ALL",
            "--user",
            "1000:1000",
            "--env",
            "HOME=/tmp",
            "--entrypoint",
            "",
            "--workdir",
            "/work",
            "--mount",
            "type=volume,source=wgsbx-j-0000000a-src,target=/work,U=true",
            "img@sha256:x",
        ];
        assert_eq!(a, want);
        no_host_reach(&a);
        assert_eq!(
            step_command(30, "npm test; rm x")[3..],
            ["wg-step", "30", "npm test; rm x"]
        );
        let wrap = &step_command(30, "x")[2];
        assert!(wrap.starts_with("timeout -k 10 \"$1\" sh -c \"$2\"") && wrap.contains(OOM_MARK));
    }

    #[test]
    fn the_service_profile_and_the_pod() {
        let svc = Service {
            name: "db".into(),
            image: "pg@sha256:y".into(),
            env: [("POSTGRES_PASSWORD".to_string(), "sandbox-only".to_string())].into(),
            data: Some("/var/lib/postgresql/data".into()),
        };
        let a = service("j-0000000a", &svc, 7, &CEILING);
        no_host_reach(&a);
        let has = |pair: [&str; 2]| a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1]);
        assert!(has(["--cap-add", SERVICE_CAPS]) && has(["--cap-drop", "ALL"]));
        assert!(has([
            "--mount",
            "type=volume,source=wgsbx-j-0000000a-svc-db,target=/var/lib/postgresql/data"
        ]));
        assert!(
            has(["--env", "POSTGRES_PASSWORD=sandbox-only"]) && has(["--pod", "wgsbx-j-0000000a"])
        );
        assert!(a.contains(&"--read-only".to_string()) && a.last().unwrap() == "pg@sha256:y");
        assert!(
            !a.contains(&"--user".to_string()),
            "services keep the image's default user"
        );
        let p = pod_create("j-0000000a", 7);
        assert!(p.windows(2).any(|w| w[0] == "--network" && w[1] == "none"));
        assert_eq!(volume_import("v"), ["volume", "import", "v", "-"]);
        assert!(leftovers("volumes", "j-1").contains(&"label=wg.job=j-1".to_string()));
        assert!(leftovers("containers", "j-1").contains(&"label=wg.job=j-1".to_string()));
        assert!(job_containers("j-1").contains(&"{{.ID}} {{.IsInfra}}".to_string()));
        assert_eq!(volume_rm(&["a".into()]), ["volume", "rm", "-f", "a"]);
        assert_eq!(rm("c"), ["rm", "-f", "--volumes", "--time", "5", "c"]);
        let e = env();
        assert!(e
            .iter()
            .any(|(k, v)| k == "PATH" && !v.contains("orbstack")));
        assert!(e
            .iter()
            .any(|(k, v)| k == "XDG_RUNTIME_DIR" && v.starts_with("/run/user/")));
    }

    #[test]
    fn the_real_runner_reports_a_missing_podman_or_input() {
        let r = RealPodman.run(
            &["--version".into()],
            Some(Path::new("/nonexistent/in")),
            Duration::from_secs(1),
        );
        assert!(!r.ok() && r.out.contains("job input"));
    }
}
