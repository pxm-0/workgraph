//! The sandbox parts of `factory.toml` (design §7.1): `[sandbox]` names the existing machine and
//! user, and `[repo.sandbox]` a repo's image, services and in-container check commands. Owner
//! local and validated at load; the factory can name none of it.

use super::job::{self, Service, Step, CEILING};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxFile {
    machine: Option<String>,
    user: Option<String>,
    probe: Option<String>,
}

/// The machine and user every sandbox call goes to. The factory never creates, resets, deletes,
/// starts or stops the machine.
#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub machine: String,
    pub user: String,
    /// The doctor's probe image (busybox), pinned by digest.
    pub probe: String,
}

/// busybox 1.37 (linux/amd64), the default probe image.
pub const PROBE: &str =
    "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceCfg {
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub data: Option<String>,
    /// Only `service` exists.
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoSandbox {
    pub image: String,
    #[serde(default = "pre_baked")]
    pub deps_image: bool,
    #[serde(default)]
    pub prepare: Option<String>,
    #[serde(default)]
    pub registry: Option<String>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceCfg>,
    /// Check name → the command inside the container (same names as the host checks).
    #[serde(default)]
    pub checks: BTreeMap<String, String>,
    /// Check name → what `project.json` holds for a tainted task (design §9.5).
    #[serde(default)]
    pub verify: BTreeMap<String, String>,
    /// The release line's rehearsal steps, run in order in one sandbox job (Phase 4), e.g.
    /// `npm run migrate:up`, `npm run migrate:down`, `npm run migrate:up`. No network, ever.
    #[serde(default)]
    pub release_rehearsal: Vec<String>,
}

fn pre_baked() -> bool {
    true
}

fn name_ok(s: &str, max: usize, first: fn(char) -> bool) -> bool {
    (1..=max).contains(&s.len())
        && s.starts_with(first)
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub fn machine(f: SandboxFile) -> Result<Machine, String> {
    let m = Machine {
        machine: f.machine.unwrap_or_else(|| "oreochiserver-sim".into()),
        user: f.user.unwrap_or_else(|| "wgsbx".into()),
        probe: f.probe.unwrap_or_else(|| PROBE.into()),
    };
    job::image_ref(&m.probe).map_err(|e| format!("[sandbox].probe: {e}"))?;
    if !name_ok(&m.machine, 64, |c| c.is_ascii_alphanumeric()) {
        return Err("[sandbox].machine is not a machine name".into());
    }
    if m.machine.ends_with("-golden") {
        return Err("[sandbox].machine may not be a golden image".into());
    }
    // The real server is never a sandbox: risky work and release rehearsals target the sim only.
    if m.machine == "oreochiserver" {
        return Err(
            "[sandbox].machine may never be the real oreochiserver; use oreochiserver-sim".into(),
        );
    }
    if !name_ok(&m.user, 32, |c| c.is_ascii_lowercase() || c == '_') || m.user == "root" {
        return Err("[sandbox].user is not an unprivileged user name".into());
    }
    Ok(m)
}

/// The verify string a tainted task's `project.json` may hold for check `name`.
pub fn verify_ok(cmd: &str, name: &str) -> bool {
    let want = format!(" sandbox exec --task {{task}} --check {name}");
    cmd.strip_suffix(&want).is_some_and(|wg| {
        wg.starts_with('/') && wg.ends_with("/wg") && !wg.contains(char::is_whitespace)
    })
}

pub fn repo(name: &str, s: &RepoSandbox) -> Result<(), String> {
    let what = |k: &str| format!("[[repo]] {name}: [repo.sandbox] {k}");
    job::image_ref(&s.image).map_err(|e| what(&e))?;
    if !s.deps_image || s.prepare.is_some() || s.registry.is_some() {
        return Err(what("deps_image = false (prepare, registry) needs the Phase 6 egress proxy; bake dependencies into the image"));
    }
    for (n, svc) in &s.services {
        if svc.profile.as_deref().is_some_and(|p| p != "service") {
            return Err(what(&format!(
                "service {n}: the only profile is \"service\""
            )));
        }
        let probe = Service {
            name: n.clone(),
            image: svc.image.clone(),
            env: svc.env.clone(),
            data: svc.data.clone(),
        };
        let j = job::Job {
            services: vec![probe],
            ..stub(&s.image)
        };
        job::validate(&j, &|_| true).map_err(|e| what(&e))?;
    }
    for (n, cmd) in &s.checks {
        crate::ids::check_name(n).map_err(|e| what(&e))?;
        job::command(cmd, &format!("check {n}")).map_err(|e| what(&e))?;
    }
    if s.release_rehearsal.len() > 8 {
        return Err(what("release_rehearsal has at most 8 steps"));
    }
    for (i, cmd) in s.release_rehearsal.iter().enumerate() {
        job::command(cmd, &format!("release_rehearsal step {}", i + 1)).map_err(|e| what(&e))?;
    }
    for (n, cmd) in &s.verify {
        if !s.checks.contains_key(n) {
            return Err(what(&format!(
                "verify.{n} has no sandbox check of that name"
            )));
        }
        if !verify_ok(cmd, n) {
            return Err(what(&format!(
                "verify.{n} must be `<abs>/wg sandbox exec --task {{task}} --check {n}`"
            )));
        }
    }
    Ok(())
}

fn stub(image: &str) -> job::Job {
    job::Job {
        v: job::PROTOCOL,
        job: "j-00000000".into(),
        nonce: "config".into(),
        image: image.into(),
        prepare: None,
        services: vec![],
        steps: vec![Step {
            name: "x".into(),
            cmd: "true".into(),
            timeout_secs: 1,
        }],
        return_patch: false,
        limits: CEILING,
    }
}

impl RepoSandbox {
    pub fn services(&self) -> Vec<Service> {
        self.services
            .iter()
            .map(|(n, s)| Service {
                name: n.clone(),
                image: s.image.clone(),
                env: s.env.clone(),
                data: s.data.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> String {
        format!("docker.io/library/node@sha256:{}", "a".repeat(64))
    }

    fn parse(extra: &str) -> Result<(), String> {
        let text = format!(
            "image = \"{}\"\nchecks = {{ test = \"npm test\" }}\n{extra}",
            img()
        );
        let s: RepoSandbox = toml::from_str(&text).map_err(|e| e.to_string())?;
        repo("o/r", &s)
    }

    #[test]
    fn machine_defaults_and_refusals() {
        assert_eq!(
            machine(SandboxFile::default()).unwrap(),
            Machine {
                machine: "oreochiserver-sim".into(),
                user: "wgsbx".into(),
                probe: PROBE.into()
            }
        );
        let f = |m: &str, u: &str| {
            machine(SandboxFile {
                machine: Some(m.into()),
                user: Some(u.into()),
                probe: None,
            })
        };
        assert!(machine(SandboxFile {
            probe: Some("busybox".into()),
            ..SandboxFile::default()
        })
        .unwrap_err()
        .contains("probe"));
        assert!(f("oreochiserver-sim-golden", "wgsbx")
            .unwrap_err()
            .contains("golden"));
        assert!(f("oreochiserver", "wgsbx")
            .unwrap_err()
            .contains("never be the real oreochiserver"));
        assert!(
            f("a b", "wgsbx").is_err() && f("sim", "root").is_err() && f("sim", "Bad").is_err()
        );
    }

    #[test]
    fn repo_sandbox_rules_one_rejection_each() {
        assert_eq!(parse(""), Ok(()));
        let pg = format!("docker.io/library/postgres@sha256:{}", "b".repeat(64));
        assert_eq!(parse(&format!("services = {{ db = {{ image = \"{pg}\", profile = \"service\", data = \"/var/lib/postgresql/data\", env = {{ POSTGRES_PASSWORD = \"sandbox-only\" }} }} }}")), Ok(()));
        assert_eq!(parse("verify = { test = \"/Users/o/.workgraph/bin/current/wg sandbox exec --task {task} --check test\" }"), Ok(()));
        assert_eq!(
            parse("release_rehearsal = [\"npm run migrate:up\", \"npm run migrate:down\"]"),
            Ok(())
        );
        for (extra, want) in [
            ("deps_image = false", "Phase 6"),
            ("prepare = \"npm ci\"", "Phase 6"),
            ("services = { db = { image = \"postgres:16\" } }", "digest"),
            (
                &format!("services = {{ db = {{ image = \"{pg}\", profile = \"step\" }} }}"),
                "profile",
            ),
            (
                "verify = { lint = \"/x/wg sandbox exec --task {task} --check lint\" }",
                "no sandbox check",
            ),
            ("verify = { test = \"npm test\" }", "must be"),
            ("release_rehearsal = [\"\"]", "release_rehearsal step 1"),
            (
                "release_rehearsal = [\"a\", \"a\", \"a\", \"a\", \"a\", \"a\", \"a\", \"a\", \"a\"]",
                "at most 8",
            ),
            (
                "verify = { test = \"wg sandbox exec --task {task} --check test\" }",
                "must be",
            ),
        ] {
            let e = parse(extra).unwrap_err();
            assert!(e.contains(want), "{extra}: {e}");
        }
        let bad_img: RepoSandbox = toml::from_str("image = \"node:22\"").unwrap();
        assert!(repo("o/r", &bad_img).is_err());
        let bad_check: RepoSandbox = toml::from_str(&format!(
            "image = \"{}\"\nchecks = {{ Test = \"x\" }}",
            img()
        ))
        .unwrap();
        assert!(repo("o/r", &bad_check).is_err());
    }
}
