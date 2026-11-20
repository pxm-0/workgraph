//! `wg sandbox doctor` (design §8.4): the Mac-side checks, then the agent's own doctor on the sim
//! (user, session, rootless podman, container isolation), plus the read-only argus fingerprint.
//! Every condition must hold or risky units park. It never starts, stops or changes the machine.

use super::client::Client;
use super::job::PROTOCOL;
use crate::config::Config;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::process::Command;

fn out(program: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(program)
        .args(args)
        .env_clear()
        .envs(crate::exec::env::scrubbed())
        .output()
        .map_err(|e| format!("{program} failed to start: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            crate::ids::one_line(&String::from_utf8_lossy(&o.stderr), 200)
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Check 1: `orbctl list --format json` shows the machine running.
pub fn machine_running(list: &str, machine: &str) -> Result<(), String> {
    let v: Value =
        serde_json::from_str(list).map_err(|_| "orbctl list printed no JSON".to_string())?;
    let m = v
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["name"] == machine);
    match m.and_then(|m| m["state"].as_str()) {
        Some("running") => Ok(()),
        Some(state) => Err(format!("sim_not_running: {machine} is {state}")),
        None => Err(format!("sim_not_running: no machine {machine}")),
    }
}

/// The verdict and every check; `failed` is the first failure's reason.
pub fn doctor(cfg: &Config) -> Value {
    let m = &cfg.sandbox.machine;
    let mut checks: Vec<(&str, Result<(), String>)> = Vec::new();
    checks.push((
        "machine_running",
        out(&cfg.orbctl_bin, &["list", "--format", "json"]).and_then(|l| machine_running(&l, m)),
    ));
    for key in ["machines.expose_ports_to_lan", "docker.expose_ports_to_lan"] {
        let r = out(&cfg.orbctl_bin, &["config", "get", key]).and_then(|v| match v.trim() {
            "false" => Ok(()),
            other => Err(format!("lan_exposed: {key} is {other:?}")),
        });
        checks.push(("lan_exposure", r));
    }
    let mut agent = Value::Null;
    if checks.iter().all(|(_, r)| r.is_ok()) {
        let r = Client::new(cfg).doctor().and_then(|a| {
            agent = a.clone();
            if a["protocol"] != json!(PROTOCOL) {
                Err(format!(
                    "agent_outdated: the agent speaks protocol {}, wg speaks {PROTOCOL}",
                    a["protocol"]
                ))
            } else if a["ok"] != true {
                Err(a["failed"]
                    .as_str()
                    .unwrap_or("the agent's doctor failed")
                    .to_string())
            } else {
                Ok(())
            }
        });
        checks.push((
            "agent",
            r.map_err(|e| {
                if e.contains("No such file") || e.contains("not found") {
                    format!("not_provisioned: {e}")
                } else {
                    e
                }
            }),
        ));
    }
    let failed = checks.iter().find_map(|(_, r)| r.clone().err());
    json!({
        "ok": failed.is_none(),
        "failed": failed,
        "machine": m,
        // The sim VM shares the Mac's reach; the owner accepted it (design §17 D1). Stated each run.
        "vm_host_reach": "accepted",
        "checks": checks.iter().map(|(n, r)| json!({ "check": n, "ok": r.is_ok(), "why": r.clone().err() })).collect::<Vec<_>>(),
        "agent": agent,
    })
}

/// Check 6: read-only, as the sim's default user. Unit-file set, enablement and failed units.
pub fn fingerprint(cfg: &Config) -> Result<Value, String> {
    let m = cfg.sandbox.machine.as_str();
    let files = out(
        &cfg.orb_bin,
        &[
            "run",
            "-m",
            m,
            "systemctl",
            "list-unit-files",
            "argus*",
            "--no-legend",
        ],
    )?;
    let failed = out(
        &cfg.orb_bin,
        &[
            "run",
            "-m",
            m,
            "systemctl",
            "list-units",
            "argus*",
            "--state=failed",
            "--no-legend",
        ],
    )?;
    Ok(fingerprint_of(&files, &failed))
}

pub fn fingerprint_of(files: &str, failed: &str) -> Value {
    let norm = |t: &str| {
        t.lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (files, failed) = (norm(files), norm(failed));
    let sha = format!(
        "{:x}",
        Sha256::digest(format!("{files}\n---\n{failed}").as_bytes())
    );
    json!({ "sha256": sha, "unit_files": files.lines().count(), "failed": failed.lines().collect::<Vec<_>>() })
}

/// A changed fingerprint between shift start and end is an incident (design §8.4 check 6).
pub fn changed(before: &Value, after: &Value) -> Option<String> {
    (before["sha256"] != after["sha256"]).then(|| {
        format!(
            "argus fingerprint changed during the shift: unit files {} -> {}, failed {} -> {}",
            before["unit_files"], after["unit_files"], before["failed"], after["failed"]
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_must_be_running_and_named() {
        let list = r#"[{"name":"oreochiserver-sim","state":"running"},{"name":"oreochiserver-sim-golden","state":"stopped"}]"#;
        assert_eq!(machine_running(list, "oreochiserver-sim"), Ok(()));
        assert!(machine_running(list, "oreochiserver-sim-golden")
            .unwrap_err()
            .contains("stopped"));
        assert!(machine_running(list, "x")
            .unwrap_err()
            .contains("no machine"));
        assert!(machine_running("nope", "x").is_err());
    }

    #[test]
    fn the_fingerprint_ignores_spacing_and_sees_new_units_and_failures() {
        let a = fingerprint_of("argus-a.service   enabled enabled\n", "");
        let same = fingerprint_of("argus-a.service enabled  enabled", "\n");
        assert_eq!(changed(&a, &same), None);
        let disabled = fingerprint_of("argus-a.service disabled enabled", "");
        assert!(changed(&a, &disabled)
            .unwrap()
            .contains("argus fingerprint changed"));
        let failing = fingerprint_of(
            "argus-a.service enabled enabled",
            "argus-a.service loaded failed failed",
        );
        assert!(changed(&a, &failing).is_some());
        assert_eq!(failing["failed"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_orbctl_fails_closed() {
        let cfg = crate::config::parse(
            "[tools]\norbctl = \"/nonexistent/orbctl\"\norb = \"/nonexistent/orb\"\n",
        )
        .unwrap();
        let d = doctor(&cfg);
        assert_eq!(d["ok"], false);
        assert_eq!(d["vm_host_reach"], "accepted");
        assert!(d["failed"].as_str().unwrap().contains("failed to start"));
        assert!(fingerprint(&cfg).is_err());
    }
}
