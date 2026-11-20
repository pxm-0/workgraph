//! `sandbox-agent doctor` (design §8.4 checks 3-5), run as wgsbx on the sim. Every condition must
//! hold or risky units park; there is no opt-out. It judges the container's isolation, not the VM's.

use super::podman::{self, Podman};
use super::probe::{self, Probe, Profile};
use super::store::{self, Allowlist};
use crate::sandbox::job::{Service, CEILING, PROTOCOL};
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

const T: Duration = Duration::from_secs(180);
pub const MIN_FREE_KB: u64 = 10 * 1024 * 1024;

/// One named check and why it failed.
pub type Check = (String, Result<(), String>);

fn cmd(program: &str, args: &[&str]) -> (bool, String) {
    match Command::new(program)
        .args(args)
        .env_clear()
        .envs(podman::env())
        .output()
    {
        Ok(o) => (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (false, format!("{program} failed to start: {e}")),
    }
}

fn status_field(key: &str) -> Option<String> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|l| l.starts_with(key))?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
}

/// `(name, start, count)` entries of `/etc/subuid`.
pub fn subid_ranges(text: &str) -> Vec<(String, u64, u64)> {
    text.lines()
        .filter_map(|l| {
            let mut p = l.split(':');
            Some((
                p.next()?.to_string(),
                p.next()?.parse().ok()?,
                p.next()?.parse().ok()?,
            ))
        })
        .collect()
}

/// The user's single range, overlapping nobody else's (an overlap shares uids with argus users).
pub fn own_range(text: &str, user: &str) -> Result<(u64, u64), String> {
    let all = subid_ranges(text);
    let mine: Vec<&(String, u64, u64)> = all.iter().filter(|r| r.0 == user).collect();
    let [(_, start, count)] = mine.as_slice() else {
        return Err(format!("{user} needs exactly one /etc/subuid range"));
    };
    if let Some(o) = all
        .iter()
        .find(|r| r.0 != user && r.1 < start + count && *start < r.1 + r.2)
    {
        return Err(format!("{user}'s subuid range overlaps {}'s", o.0));
    }
    Ok((*start, *count))
}

fn check(name: &str, ok: bool, why: impl Into<String>) -> Check {
    (name.into(), if ok { Ok(()) } else { Err(why.into()) })
}

/// Checks 3 and 4: the user, its session, rootless podman, delegation, space, images, root files.
pub fn host(allow: &Result<Allowlist, String>) -> Vec<Check> {
    let uid = status_field("Uid:").unwrap_or_default();
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let entry: Vec<&str> = passwd
        .lines()
        .find(|l| l.split(':').nth(2) == Some(uid.as_str()))
        .unwrap_or("")
        .split(':')
        .collect();
    let user = entry.first().copied().unwrap_or("");
    let (_, groups) = cmd("id", &["-Gn"]);
    let (sudo_ok, _) = cmd("sudo", &["-n", "true"]);
    let (_, pw) = cmd("passwd", &["-S", user]);
    let (_, running) = cmd("systemctl", &["--user", "is-system-running"]);
    let cg =
        format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers");
    let controllers = std::fs::read_to_string(&cg).unwrap_or_default();
    let (info_ok, info) = cmd("podman", &["info", "--format", "json"]);
    let info: Value = serde_json::from_str(&info).unwrap_or(json!({}));
    let range = own_range(
        &std::fs::read_to_string("/etc/subuid").unwrap_or_default(),
        user,
    );
    let maps_range = range.as_ref().is_ok_and(|(start, _)| {
        info["host"]["idMappings"]["uidmap"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|m| m["host_id"].as_u64() == Some(*start))
    });
    let home = std::env::var("HOME").unwrap_or_default();
    let (_, df) = cmd("df", &["-Pk", &home]);
    let free = df
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let exe = std::env::current_exe().map_err(|e| e.to_string());
    let mut out = vec![
        check("uid", uid == "1100", format!("runs as uid {uid}, not 1100")),
        check(
            "groups",
            !groups
                .split_whitespace()
                .any(|g| g == "sudo" || g == "docker" || g == "wheel"),
            format!("in groups {}", groups.trim()),
        ),
        check("sudo", !sudo_ok, "sudo -n true succeeded"),
        check(
            "shell",
            entry.get(6) == Some(&"/bin/bash"),
            "the login shell is not /bin/bash",
        ),
        check(
            "password",
            pw.split_whitespace().nth(1) == Some("L"),
            "the password is not locked",
        ),
        check(
            "linger",
            std::path::Path::new(&format!("/var/lib/systemd/linger/{user}")).exists(),
            "lingering is off",
        ),
        check(
            "runtime_dir",
            std::path::Path::new(&format!("/run/user/{uid}")).is_dir(),
            "no /run/user/<uid>",
        ),
        check(
            "user_session",
            matches!(running.trim(), "running" | "degraded"),
            format!("systemctl --user is {}", running.trim()),
        ),
        check(
            "rootless",
            info_ok && info["host"]["security"]["rootless"] == true,
            "podman is not rootless",
        ),
        (String::from("subuid"), range.clone().map(|_| ())),
        check(
            "subuid_mapped",
            maps_range,
            "podman does not map the user's subuid range",
        ),
        check(
            "cgroup_delegation",
            ["cpu", "memory", "pids"]
                .iter()
                .all(|c| controllers.split_whitespace().any(|x| x == *c)),
            format!("{cg} lacks cpu memory pids"),
        ),
        check(
            "disk",
            free >= MIN_FREE_KB,
            format!("{free} KB free under {home}, need 10 GB"),
        ),
        (
            String::from("agent_root_owned"),
            exe.and_then(|p| store::root_owned(&p)),
        ),
        (String::from("allowlist"), allow.clone().map(|_| ())),
    ];
    if let Ok(a) = allow {
        for img in a.images.iter().chain([&a.probe]) {
            let (present, _) = cmd("podman", &["image", "exists", img]);
            out.push(check("image", present, format!("{img} is not pulled")));
        }
    }
    out
}

/// A test build of the agent can plant a breach in the probe, to prove the doctor catches it.
#[cfg(debug_assertions)]
fn plant(args: &mut Vec<String>, what: &str, script: &mut String) {
    let at = |a: &mut Vec<String>, extra: &[&str]| {
        for (i, x) in extra.iter().enumerate() {
            a.insert(1 + i, x.to_string());
        }
    };
    match what {
        "bind" if args[0] == "run" => {
            at(args, &["--mount", "type=bind,source=/tmp,target=/Users,ro"])
        }
        "macctl" if args[0] == "run" => {
            at(args, &["--tmpfs", "/opt/plant:rw,exec,mode=1777"]);
            *script =
                format!("cp /bin/busybox /opt/plant/macctl; chmod 755 /opt/plant/macctl; {script}");
        }
        "cap" if args[0] == "run" => at(args, &["--cap-add", "NET_RAW"]),
        "network" if args[0] == "pod" => {
            if let Some(i) = args.iter().position(|x| x == "none") {
                args[i] = "host".into();
            }
        }
        _ => {}
    }
}

#[cfg(not(debug_assertions))]
fn plant(_: &mut Vec<String>, _: &str, _: &mut String) {}

fn inspect(pm: &dyn Podman, args: Vec<String>) -> Value {
    serde_json::from_str(pm.run(&args, None, T).out.trim()).unwrap_or(Value::Null)
}

/// Check 5: probe containers with exactly the step and service profiles, judged inside and out.
pub fn container(
    pm: &dyn Podman,
    probe_image: &str,
    job: &str,
    created: u64,
    planted: &str,
) -> Vec<Check> {
    let l = CEILING;
    let svc = Service {
        name: "probe".into(),
        image: probe_image.into(),
        env: Default::default(),
        data: Some("/data".into()),
    };
    let vols = vec![podman::src_volume(job), podman::data_volume(job, "probe")];
    let mut checks: Vec<Check> = Vec::new();
    let mut setup = vec![podman::pod_create(job, created)];
    setup.extend(vols.iter().map(|v| podman::volume_create(v, job, created)));
    let mut ready = Ok(());
    for mut a in setup {
        plant(&mut a, planted, &mut String::new());
        let r = pm.run(&a, None, T);
        if !r.ok() {
            ready = Err(format!(
                "container_isolation_failed:setup: {}",
                crate::ids::one_line(&r.out, 200)
            ));
            break;
        }
    }
    if ready.is_ok() {
        let runs = [
            (
                Profile::Step,
                podman::step(job, &format!("wgsbx-{job}-probe"), probe_image, created, &l),
                format!("wgsbx-{job}-probe"),
            ),
            (
                Profile::Service,
                podman::service(job, &svc, created, &l),
                format!("wgsbx-{job}-probe-svc"),
            ),
        ];
        for (profile, mut args, name) in runs {
            if profile == Profile::Service {
                // The service builder names it after the service; give the probe its own name.
                let i = args.iter().position(|x| x == "--name").expect("named") + 1;
                args[i] = name.clone();
            }
            let mut script = probe::SCRIPT.to_string();
            plant(&mut args, planted, &mut script);
            args.extend(["sh".to_string(), "-c".into(), script]);
            let r = pm.run(&args, None, T);
            let label = format!(
                "probe_{}",
                if profile == Profile::Step {
                    "step"
                } else {
                    "service"
                }
            );
            let inside = probe::judge(&Probe::parse(&r.out), profile, job, &l);
            let outside = probe::judge_inspect(&inspect(pm, podman::inspect(&name)), job);
            checks.push((label, inside.and(outside)));
        }
        let pod = inspect(
            pm,
            vec![
                "pod".into(),
                "inspect".into(),
                podman::pod(job),
                "--format".into(),
                "json".into(),
            ],
        );
        let pod = if pod.is_array() { pod[0].clone() } else { pod };
        let infra = pod["InfraContainerID"]
            .as_str()
            .unwrap_or("none")
            .to_string();
        checks.push((
            "pod_network".into(),
            probe::judge_network(&inspect(pm, podman::inspect(&infra))),
        ));
    } else {
        checks.push(("probe_setup".into(), ready));
    }
    let c = super::run::cleanup(pm, job, &vols);
    checks.push(check(
        "probe_cleanup",
        c.containers_left == 0 && c.volumes_left == 0,
        format!(
            "{} containers and {} volumes left",
            c.containers_left, c.volumes_left
        ),
    ));
    checks
}

/// The whole report; `failed` names the first failing check (`not_provisioned` when unprovisioned).
pub fn report(checks: &[Check]) -> Value {
    let first = checks.iter().find(|(_, r)| r.is_err());
    let failed = first.map(|(name, r)| {
        let why = r.clone().unwrap_err();
        if why.starts_with("container_isolation_failed:") || why.starts_with("not_provisioned") {
            why
        } else {
            format!("{name}: {why}")
        }
    });
    json!({
        "ok": first.is_none(),
        "protocol": PROTOCOL,
        "failed": failed,
        "checks": checks.iter().map(|(n, r)| json!({ "check": n, "ok": r.is_ok(), "why": r.clone().err() })).collect::<Vec<_>>(),
    })
}

pub fn doctor(pm: &dyn Podman, planted: &str) -> Value {
    let allow = store::allowlist();
    let mut checks = host(&allow);
    match &allow {
        Ok(a) => {
            let job = match crate::tokens_store::random_hex(8) {
                Ok(h) => format!("j-{h}"),
                Err(e) => return report(&[("random".into(), Err(e))]),
            };
            checks.extend(container(
                pm,
                &a.probe,
                &job,
                crate::journal::now(),
                planted,
            ));
        }
        Err(e) => checks.push(("probe".into(), Err(e.clone()))),
    }
    report(&checks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::agent::run::tests::{ran, Fake};
    use std::sync::Mutex;

    #[test]
    fn subuid_ranges_must_be_single_and_disjoint() {
        let t = "oreo:100000:65536\nargus-work-sandbox:231072:65536\nwgsbx:362144:65536\n";
        assert_eq!(own_range(t, "wgsbx"), Ok((362144, 65536)));
        let clash = "argus-personal-sandbox:165536:65536\nwgsbx:200000:65536\n";
        assert!(own_range(clash, "wgsbx")
            .unwrap_err()
            .contains("overlaps argus-personal-sandbox"));
        assert!(own_range("oreo:1:2\n", "wgsbx")
            .unwrap_err()
            .contains("exactly one"));
    }

    #[test]
    fn the_probe_runs_both_profiles_and_cleans_up() {
        let good = |caps: &str, id: &str| {
            format!("uid={id}\ngid={id}\nnet=lo\nnc=present\nCapPrm={caps}\nCapEff={caps}\nCapBnd={caps}\nNoNewPrivs=1\nmemory_max=2147483648\npids_max=512\ncpu_max=200000/100000\nuid_map=0/1100/1\n---mountinfo\n1 0 0:1 / / ro - overlay o rw\n2 1 0:2 / /tmp rw - tmpfs tmpfs rw,size=524288k\n")
        };
        let pm = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![
                (
                    "wgsbx-j-1-probe-svc --pod",
                    ran(0, &good(probe::SERVICE_CAP_MASK, "0")),
                ),
                (
                    "wgsbx-j-1-probe --pod",
                    ran(0, &good("0000000000000000", "1000")),
                ),
                (
                    "inspect wgsbx-j-1-probe",
                    ran(
                        0,
                        r#"[{"Mounts":[{"Type":"volume","Name":"wgsbx-j-1-src"}]}]"#,
                    ),
                ),
                ("pod inspect", ran(0, r#"[{"InfraContainerID":"inf"}]"#)),
                (
                    "inspect inf",
                    ran(0, r#"[{"HostConfig":{"NetworkMode":"none"}}]"#),
                ),
            ],
        };
        let checks = container(&pm, "busybox@sha256:x", "j-1", 7, "");
        let r = report(&checks);
        assert_eq!(r["ok"], true, "{r}");
        assert!(pm
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "pod rm -f --time 5 wgsbx-j-1"));
        let bad_net = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![(
                "inspect",
                ran(0, r#"[{"HostConfig":{"NetworkMode":"host"},"Mounts":[]}]"#),
            )],
        };
        let r = report(&container(&bad_net, "b@sha256:x", "j-1", 7, ""));
        assert_eq!(r["ok"], false);
        assert!(r["failed"]
            .as_str()
            .unwrap()
            .starts_with("container_isolation_failed:"));
        let no_pod = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![("pod create", ran(125, "no podman"))],
        };
        let r = report(&container(&no_pod, "b@sha256:x", "j-1", 7, ""));
        assert!(r["failed"].as_str().unwrap().contains("setup"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_test_build_plants_each_breach_in_the_probe_only() {
        let mut s = String::from("echo");
        let mut a: Vec<String> = ["run", "--name", "x", "img"]
            .iter()
            .map(|x| x.to_string())
            .collect();
        plant(&mut a, "bind", &mut s);
        assert_eq!(
            a[1..3],
            [
                "--mount".to_string(),
                "type=bind,source=/tmp,target=/Users,ro".into()
            ]
        );
        let mut p: Vec<String> = ["pod", "create", "--network", "none"]
            .iter()
            .map(|x| x.to_string())
            .collect();
        plant(&mut p, "network", &mut s);
        assert_eq!(p[3], "host");
        plant(&mut a, "macctl", &mut s);
        assert!(s.starts_with("cp /bin/busybox /opt/plant/macctl"));
        let mut c = vec!["run".to_string()];
        plant(&mut c, "cap", &mut s);
        assert_eq!(c, ["run", "--cap-add", "NET_RAW"]);
    }

    #[test]
    fn host_checks_name_every_condition() {
        let names: Vec<String> = host(&Err("not_provisioned: no allowlist".into()))
            .into_iter()
            .map(|c| c.0)
            .collect();
        for n in [
            "uid",
            "groups",
            "sudo",
            "shell",
            "password",
            "linger",
            "user_session",
            "rootless",
            "subuid",
            "cgroup_delegation",
            "disk",
            "agent_root_owned",
            "allowlist",
        ] {
            assert!(names.iter().any(|x| x == n), "{n}");
        }
        let r = report(&[("allowlist".into(), Err("not_provisioned: x".into()))]);
        assert_eq!(r["failed"], "not_provisioned: x");
    }
}
