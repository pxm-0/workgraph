//! The sandbox against real rootless podman (design §15.4), run by CI's `podman` job on
//! ubuntu-latest with `WG_PODMAN=1` and a root-owned `/etc/wgsbx/images.toml` (busybox as the probe,
//! busybox and postgres as images). Elsewhere every test skips. It drives `wg sandbox-agent`
//! directly, the same binary the sim runs.

use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wg::sandbox::agent::podman::RealPodman;
use wg::sandbox::agent::store::Allowlist;
use wg::sandbox::job::{Job, JobResult, Service, Step, CEILING, PROTOCOL};

const WG: &str = env!("CARGO_BIN_EXE_wg");

fn enabled() -> bool {
    let on = std::env::var("WG_PODMAN").is_ok();
    if !on {
        eprintln!(
            "skipped: set WG_PODMAN=1 (with rootless podman and /etc/wgsbx/images.toml) to run"
        );
    }
    on
}

fn allow() -> Allowlist {
    Allowlist::parse(&std::fs::read_to_string("/etc/wgsbx/images.toml").unwrap()).unwrap()
}

fn image(part: &str) -> String {
    allow()
        .images
        .into_iter()
        .find(|i| i.contains(part))
        .unwrap_or_else(|| panic!("no {part} image allowlisted"))
}

fn hex() -> String {
    wg::tokens_store::random_hex(8).unwrap()
}

fn job(img: &str, steps: &[(&str, &str, u32)]) -> Job {
    Job {
        v: PROTOCOL,
        job: format!("j-{}", hex()),
        nonce: "ci".into(),
        image: img.into(),
        prepare: None,
        services: vec![],
        steps: steps
            .iter()
            .map(|(n, c, t)| Step {
                name: n.to_string(),
                cmd: c.to_string(),
                timeout_secs: *t,
            })
            .collect(),
        return_patch: false,
        limits: CEILING,
    }
}

fn work() -> PathBuf {
    let d = std::env::temp_dir().join(format!("wg-podman-{}", hex()));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("hello.txt"), "hi\n").unwrap();
    d
}

fn stream(j: &Job) -> Vec<u8> {
    let d = work();
    let mut s = serde_json::to_vec(j).unwrap();
    s.push(b'\n');
    s.extend(wg::sandbox::bundle::tar(&d, &["hello.txt".to_string()]).unwrap());
    s
}

/// `wg sandbox-agent <sub>` with `stdin`; the last stdout line as JSON.
fn agent(args: &[&str], stdin: &[u8]) -> Value {
    let mut c = Command::new(WG)
        .arg("sandbox-agent")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(stdin).unwrap();
    let o = c.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    let line = text.lines().last().unwrap_or("");
    serde_json::from_str(line)
        .unwrap_or_else(|_| panic!("not JSON: {text} {}", String::from_utf8_lossy(&o.stderr)))
}

fn run(j: &Job) -> JobResult {
    let v = agent(&["run"], &stream(j));
    let r: JobResult = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}: {v}"));
    assert_eq!(
        (r.cleanup.containers_left, r.cleanup.volumes_left),
        (0, 0),
        "0 leftovers: {v}"
    );
    r
}

fn podman(args: &[&str]) -> String {
    let o = Command::new("podman").args(args).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn no_network_and_no_host_paths() {
    if !enabled() {
        return;
    }
    let r = run(&job(
        &image("busybox"),
        &[
            ("dns", "nslookup example.com", 30),
            ("http", "wget -q -T 3 -O /dev/null http://example.com", 30),
            ("ifaces", "test \"$(ls /sys/class/net)\" = lo", 30),
            ("paths", "test ! -e /Users && test ! -e /home/runner && test ! -e /etc/argus && cat /work/hello.txt", 30),
            ("user", "test \"$(id -u):$(id -g)\" = 1000:1000", 30),
        ],
    ));
    let exits: Vec<i64> = r.steps.iter().map(|s| s.exit).collect();
    assert!(exits[0] != 0 && exits[1] != 0, "no network: {r:?}");
    assert_eq!(&exits[2..], [0, 0, 0], "{r:?}");
    assert!(r.steps[3].tail.contains("hi"), "the work arrived as a copy");
}

#[test]
fn root_is_read_only_and_tmp_and_work_are_writable() {
    if !enabled() {
        return;
    }
    let r = run(&job(
        &image("busybox"),
        &[
            ("root", "touch /x", 30),
            ("tmp", "touch /tmp/x && echo ok > /work/new.txt", 30),
        ],
    ));
    assert_ne!(r.steps[0].exit, 0);
    assert_eq!(r.steps[1].exit, 0, "{r:?}");
}

#[test]
fn oom_pids_and_timeout_hold() {
    if !enabled() {
        return;
    }
    let mut j = job(
        &image("busybox"),
        &[
            ("oom", "head -c 400m /dev/zero | tail > /dev/null", 60),
            (
                "pids",
                "for i in $(seq 1 80); do sleep 20 & done; echo spawned-all",
                60,
            ),
            ("slow", "sleep 120", 3),
        ],
    );
    j.limits.memory_mb = 64;
    j.limits.pids = 32;
    let r = run(&j);
    assert_eq!((r.steps[0].exit, r.steps[0].oom), (137, true), "{r:?}");
    let pids = &r.steps[1];
    assert!(
        !pids.tail.contains("spawned-all") && pids.tail.to_lowercase().contains("fork"),
        "the pids limit holds: {pids:?}"
    );
    assert_eq!(
        (r.steps[2].exit, r.steps[2].timed_out),
        (124, true),
        "{r:?}"
    );
}

#[test]
fn a_postgres_service_starts_under_the_service_profile() {
    if !enabled() {
        return;
    }
    let pg = image("postgres");
    let mut j = job(
        &pg,
        &[(
            "ready",
            "for i in $(seq 1 60); do pg_isready -h 127.0.0.1 && exit 0; sleep 1; done; exit 1",
            120,
        )],
    );
    j.services = vec![Service {
        name: "db".into(),
        image: pg.clone(),
        env: [("POSTGRES_PASSWORD".to_string(), "sandbox-only".to_string())].into(),
        data: Some("/var/lib/postgresql/data".into()),
    }];
    let r = run(&j);
    assert_eq!(r.steps[0].exit, 0, "{r:?}");
}

/// Volumes no container uses and no job labels: the anonymous ones a `VOLUME` image leaks. A
/// concurrent job's anonymous volumes are in use, and its named ones are labelled.
fn dangling_anonymous() -> std::collections::BTreeSet<String> {
    let labelled = podman(&["volume", "ls", "-q", "--filter", "label=wg.job"]);
    let labelled: Vec<&str> = labelled.lines().collect();
    podman(&["volume", "ls", "-q", "--filter", "dangling=true"])
        .lines()
        .filter(|v| !labelled.contains(v))
        .map(str::to_string)
        .collect()
}

/// New dangling anonymous volumes since `before` that are still there 3 s later (a concurrent
/// `rm --volumes` can briefly show one).
fn leaked_since(before: &std::collections::BTreeSet<String>) -> Vec<String> {
    let first = dangling_anonymous();
    if first.is_subset(before) {
        return vec![];
    }
    std::thread::sleep(Duration::from_secs(3));
    let again = dangling_anonymous();
    first
        .difference(before)
        .filter(|v| again.contains(*v))
        .cloned()
        .collect()
}

#[test]
fn an_image_volume_leaves_no_anonymous_volume_behind() {
    if !enabled() {
        return;
    }
    let pg = image("postgres");
    let before = dangling_anonymous();
    // Both the step and the data-less service run an image that declares a `VOLUME`.
    let mut j = job(&pg, &[("t", "true", 30)]);
    j.services = vec![Service {
        name: "db".into(),
        image: pg.clone(),
        env: [("POSTGRES_PASSWORD".to_string(), "sandbox-only".to_string())].into(),
        data: None,
    }];
    let r = run(&j);
    assert_eq!(r.steps[0].exit, 0, "{r:?}");
    let dir = PathBuf::from(std::env::var("HOME").unwrap())
        .join("jobs")
        .join(&j.job);
    assert!(
        dir.join("job.json").exists() && !dir.join("src.tar").exists(),
        "the tar goes once imported"
    );
    let leaked = leaked_since(&before);
    assert!(leaked.is_empty(), "anonymous volumes left: {leaked:?}");
}

#[test]
fn the_reaper_spares_an_unlabelled_decoy() {
    if !enabled() {
        return;
    }
    let bb = image("busybox");
    let (decoy, old) = (format!("decoy-{}", hex()), format!("j-{}", hex()));
    let before = dangling_anonymous();
    podman(&["create", "--name", &decoy, &bb, "true"]);
    podman(&[
        "create",
        "--name",
        &format!("wgsbx-{old}-x"),
        "--label",
        &format!("wg.job={old}"),
        "--label",
        "wg.created=1",
        // postgres declares a `VOLUME`: the reaper must take the anonymous volume with it.
        &image("postgres"),
        "true",
    ]);
    let v = agent(&["reap", "--older-than", "3600"], &[]);
    assert!(v["removed"]["containers"].as_u64().unwrap() >= 1, "{v}");
    assert!(
        podman(&["ps", "-a", "--format", "{{.Names}}"]).contains(&decoy),
        "the decoy stays"
    );
    assert!(!podman(&["ps", "-a", "--format", "{{.Names}}"]).contains(&old));
    let leaked = leaked_since(&before);
    assert!(leaked.is_empty(), "anonymous volumes left: {leaked:?}");
    podman(&["rm", "-f", &decoy]);
}

#[test]
fn submit_returns_at_once_poll_waits_and_a_killed_agent_still_cleans_up() {
    if !enabled() {
        return;
    }
    let j = job(&image("busybox"), &[("long", "sleep 20", 60)]);
    let t = Instant::now();
    let v = agent(&["submit"], &stream(&j));
    assert_eq!(v["submitted"], true, "{v}");
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "submit does not wait for the job"
    );
    let p = agent(&["poll", "--job", &j.job], &[]);
    assert_eq!(p["pending"], true, "{p}");
    let done = loop {
        let t = Instant::now();
        let p = agent(&["poll", "--job", &j.job], &[]);
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "every poll returns quickly"
        );
        if p["pending"] != true {
            break p;
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    assert_eq!(done["steps"][0]["exit"], 0, "{done}");

    // The agent is killed mid-job: the unit's ExecStopPost still removes everything.
    let k = job(&image("busybox"), &[("long", "sleep 60", 120)]);
    assert_eq!(agent(&["submit"], &stream(&k))["submitted"], true);
    std::thread::sleep(Duration::from_secs(8));
    let unit = format!("wgsbx-{}", k.job);
    let env: Vec<(String, String)> = wg::sandbox::agent::podman::env();
    assert!(Command::new("systemctl")
        .args(["--user", "kill", "--signal=SIGKILL", &unit])
        .envs(env)
        .status()
        .unwrap()
        .success());
    let end = Instant::now();
    let lost = loop {
        let p = agent(&["poll", "--job", &k.job], &[]);
        if p["pending"] != true {
            break p;
        }
        assert!(end.elapsed() < Duration::from_secs(120));
        std::thread::sleep(Duration::from_secs(2));
    };
    assert!(
        lost["error"]
            .as_str()
            .unwrap_or("")
            .contains("without a result"),
        "{lost}"
    );
    assert_eq!(
        lost["cleanup"],
        serde_json::json!({ "containers_left": 0, "volumes_left": 0 }),
        "{lost}"
    );
}

#[test]
fn the_container_doctor_passes_and_fails_closed_on_each_planted_breach() {
    if !enabled() {
        return;
    }
    let probe = allow().probe;
    let ok = |plant: &str| {
        let checks = wg::sandbox::agent::doctor::container(
            &RealPodman,
            &probe,
            &format!("j-{}", hex()),
            wg::journal::now(),
            plant,
        );
        wg::sandbox::agent::doctor::report(&checks)
    };
    let clean = ok("");
    assert_eq!(clean["ok"], true, "{clean}");
    for (plant, check) in [
        ("bind", "mounts"),
        ("macctl", "mac_commands"),
        ("network", "network"),
        ("cap", "capabilities"),
    ] {
        let r = ok(plant);
        assert_eq!(r["ok"], false, "{plant}: {r}");
        let whys: Vec<&str> = r["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["why"].as_str())
            .collect();
        let want = format!("container_isolation_failed:{check}");
        assert!(whys.iter().any(|w| w.contains(&want)), "{plant}: {r}");
    }
}
