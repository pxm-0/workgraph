//! The sandbox job (design §8.5 step 3): what the Mac sends, what the agent runs, and what comes
//! back. Both sides validate independently: the agent against its own ceilings and the root-owned
//! image allowlist, the Mac against the job it sent (id, nonce, step names, sizes).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The job protocol version; the doctor reports `agent_outdated` when the two sides differ.
pub const PROTOCOL: u32 = 1;
/// The whole result line.
pub const MAX_RESULT: usize = 256 * 1024;
/// One step's output tail.
pub const MAX_TAIL: usize = 8 * 1024;
pub const TAIL_LINES: usize = 40;
/// A returned patch, decoded.
pub const MAX_PATCH: usize = 2 * 1024 * 1024;
/// The job line itself.
pub const MAX_JOB_LINE: usize = 64 * 1024;
pub const MAX_STEPS: usize = 8;
pub const MAX_SERVICES: usize = 3;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub cpus: u32,
    pub memory_mb: u32,
    pub pids: u32,
    pub tmp_mb: u32,
    pub wall_secs: u32,
}

/// The agent's own ceilings (design §8.3): no job may ask for more.
pub const CEILING: Limits = Limits {
    cpus: 2,
    memory_mb: 2048,
    pids: 512,
    tmp_mb: 512,
    wall_secs: 1200,
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    /// One argv element handed to `sh -c` inside the container; it never meets a host shell.
    pub cmd: String,
    pub timeout_secs: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The image's data directory, given a per-job writable named volume (e.g. postgres).
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    pub cmd: String,
    pub timeout_secs: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub v: u32,
    pub job: String,
    pub nonce: String,
    pub image: String,
    /// `deps_image = false` only; refused until the Phase 6 egress proxy exists (design §8.3).
    #[serde(default)]
    pub prepare: Option<Prepare>,
    #[serde(default)]
    pub services: Vec<Service>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub return_patch: bool,
    pub limits: Limits,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StepResult {
    pub name: String,
    pub exit: i64,
    pub timed_out: bool,
    pub oom: bool,
    pub tail: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Cleanup {
    pub containers_left: u32,
    pub volumes_left: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub job: String,
    pub nonce: String,
    pub steps: Vec<StepResult>,
    /// Base64 of `git diff --binary`, when the job asked for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    pub cleanup: Cleanup,
    /// Why the job stopped before its steps finished (the sandbox failed, not the code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl JobResult {
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.steps.iter().all(|s| s.exit == 0)
    }
}

/// `j-` plus 8 to 32 lowercase hex digits.
pub fn job_id(s: &str) -> Result<(), String> {
    let ok = s.strip_prefix("j-").is_some_and(|h| {
        (8..=32).contains(&h.len())
            && h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    });
    ok.then_some(())
        .ok_or_else(|| format!("{:?} is not a job id", crate::ids::truncate(s, 40)))
}

fn token(s: &str, max: usize, extra: &str) -> bool {
    (1..=max).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// `<registry>/<path>@sha256:<64 hex>`: always by digest, never a tag alone.
pub fn image_ref(s: &str) -> Result<(), String> {
    let bad = || {
        format!(
            "{:?} is not an image pinned by digest",
            crate::ids::truncate(s, 80)
        )
    };
    let (name, digest) = s.split_once("@sha256:").ok_or_else(bad)?;
    let digest_ok = digest.len() == 64
        && digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    let name_ok = token(name, 200, "./_-:")
        && name.contains('/')
        && !name.starts_with(['/', '.', '-'])
        && !name.contains("..");
    (digest_ok && name_ok).then_some(()).ok_or_else(bad)
}

/// A command string for `sh -c` inside the container: one line, no control characters.
pub fn command(s: &str, what: &str) -> Result<(), String> {
    if s.trim().is_empty() || s.len() > 4096 || s.chars().any(char::is_control) {
        return Err(format!("{what} must be one line of at most 4096 bytes"));
    }
    Ok(())
}

fn limits(l: &Limits) -> Result<(), String> {
    let c = CEILING;
    let pairs = [
        ("cpus", l.cpus, c.cpus),
        ("memory_mb", l.memory_mb, c.memory_mb),
        ("pids", l.pids, c.pids),
        ("tmp_mb", l.tmp_mb, c.tmp_mb),
        ("wall_secs", l.wall_secs, c.wall_secs),
    ];
    match pairs.iter().find(|(_, v, max)| *v == 0 || v > max) {
        Some((name, v, max)) => Err(format!("limit {name} = {v} is outside 1..={max}")),
        None => Ok(()),
    }
}

fn service(s: &Service, allowed: &dyn Fn(&str) -> bool) -> Result<(), String> {
    let name_ok = token(&s.name, 16, "-") && s.name.starts_with(|c: char| c.is_ascii_lowercase());
    if !name_ok || s.name.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(format!(
            "service name {:?} is not valid",
            crate::ids::truncate(&s.name, 20)
        ));
    }
    image_ref(&s.image)?;
    if !allowed(&s.image) {
        return Err(format!("service {} image is not in the allowlist", s.name));
    }
    for (k, v) in &s.env {
        let key_ok = token(k, 64, "_")
            && k.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
            && !k.chars().any(|c| c.is_ascii_lowercase());
        if !key_ok || v.len() > 256 || v.chars().any(char::is_control) {
            return Err(format!("service {} has a bad env entry", s.name));
        }
    }
    if let Some(d) = &s.data {
        if !d.starts_with('/') || d.contains("..") || !token(d, 128, "/._-") || d == "/" {
            return Err(format!("service {} data path is not valid", s.name));
        }
    }
    Ok(())
}

/// Every rule a job must meet; `allowed` answers for the image allowlist.
pub fn validate(j: &Job, allowed: &dyn Fn(&str) -> bool) -> Result<(), String> {
    if j.v != PROTOCOL {
        return Err(format!("job protocol {} is not {PROTOCOL}", j.v));
    }
    job_id(&j.job)?;
    if !token(&j.nonce, 64, ":_#.-") {
        return Err("the nonce is not valid".into());
    }
    image_ref(&j.image)?;
    if !allowed(&j.image) {
        return Err("the image is not in the allowlist".into());
    }
    if j.prepare.is_some() {
        return Err(
            "prepare (deps_image = false) needs the registry-only egress proxy of Phase 6".into(),
        );
    }
    limits(&j.limits)?;
    if j.services.len() > MAX_SERVICES {
        return Err(format!("at most {MAX_SERVICES} services"));
    }
    for (i, s) in j.services.iter().enumerate() {
        service(s, allowed)?;
        if j.services[..i].iter().any(|x| x.name == s.name) {
            return Err(format!("service {} is listed twice", s.name));
        }
    }
    if j.steps.is_empty() || j.steps.len() > MAX_STEPS {
        return Err(format!("a job has 1..={MAX_STEPS} steps"));
    }
    for s in &j.steps {
        crate::ids::check_name(&s.name).map_err(|e| format!("step name: {e}"))?;
        command(&s.cmd, &format!("step {}", s.name))?;
        if s.timeout_secs == 0 || s.timeout_secs > j.limits.wall_secs {
            return Err(format!(
                "step {} timeout is outside the job's wall time",
                s.name
            ));
        }
    }
    Ok(())
}

/// The last 40 lines and at most 8 KB of `text`, cut on a character boundary.
pub fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let t = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    if t.len() <= MAX_TAIL {
        return t;
    }
    let mut start = t.len() - MAX_TAIL;
    while !t.is_char_boundary(start) {
        start += 1;
    }
    t[start..].to_string()
}

/// What a poll answers: still running, or the result.
#[derive(Debug, PartialEq)]
pub enum Polled {
    Pending,
    Done(JobResult),
}

/// The Mac's check of the agent's line (design §8.5 step 7): untrusted until it matches the job
/// sent, field by field and size by size.
pub fn check_result(line: &str, sent: &Job) -> Result<Polled, String> {
    if line.len() > MAX_RESULT {
        return Err("the sandbox result is over 256 KB".into());
    }
    let v: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|_| "the sandbox result is not JSON".to_string())?;
    if v["pending"] == true {
        return match v["job"].as_str() {
            Some(j) if j == sent.job => Ok(Polled::Pending),
            _ => Err("a pending answer for another job".into()),
        };
    }
    let r: JobResult =
        serde_json::from_value(v).map_err(|e| format!("the sandbox result is malformed: {e}"))?;
    if r.job != sent.job || r.nonce != sent.nonce {
        return Err("the sandbox result is for another job or nonce".into());
    }
    let names: Vec<&str> = sent.steps.iter().map(|s| s.name.as_str()).collect();
    let got: Vec<&str> = r.steps.iter().map(|s| s.name.as_str()).collect();
    let complete = got == names;
    let truncated = r.error.is_some() && names.starts_with(&got);
    if !complete && !truncated {
        return Err("the sandbox result's steps do not match the job".into());
    }
    if r.steps
        .iter()
        .any(|s| s.tail.len() > MAX_TAIL || s.tail.lines().count() > TAIL_LINES)
    {
        return Err("a step tail is over 40 lines or 8 KB".into());
    }
    if r.patch.is_some() && !sent.return_patch {
        return Err("a patch came back that the job did not ask for".into());
    }
    if r.patch
        .as_ref()
        .is_some_and(|p| p.len() > MAX_PATCH * 4 / 3 + 8)
    {
        return Err("the returned patch is over 2 MB".into());
    }
    if let Some(e) = &r.error {
        if e.len() > 2000 || e.contains('\n') {
            return Err("the sandbox error is not one short line".into());
        }
    }
    Ok(Polled::Done(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: &str =
        "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";

    pub fn job() -> Job {
        Job {
            v: PROTOCOL,
            job: "j-7c1e2a9b".into(),
            nonce: "r-3f9a1c:U2:sbx:3".into(),
            image: IMG.into(),
            prepare: None,
            services: vec![Service {
                name: "db".into(),
                image: IMG.into(),
                env: [("POSTGRES_PASSWORD".to_string(), "sandbox-only".to_string())].into(),
                data: Some("/var/lib/postgresql/data".into()),
            }],
            steps: vec![Step {
                name: "test".into(),
                cmd: "npm test -- db".into(),
                timeout_secs: 600,
            }],
            return_patch: false,
            limits: CEILING,
        }
    }

    #[test]
    fn a_good_job_passes_and_each_rule_rejects_one_change() {
        let yes = |_: &str| true;
        assert_eq!(validate(&job(), &yes), Ok(()));
        type Change = Box<dyn Fn(&mut Job)>;
        let cases: Vec<(Change, &str)> = vec![
            (Box::new(|j| j.v = 2), "protocol"),
            (Box::new(|j| j.job = "j-XYZ".into()), "job id"),
            (Box::new(|j| j.nonce = "a b".into()), "nonce"),
            (
                Box::new(|j| j.image = "docker.io/library/node:22".into()),
                "digest",
            ),
            (
                Box::new(|j| {
                    j.prepare = Some(Prepare {
                        cmd: "npm ci".into(),
                        timeout_secs: 60,
                    })
                }),
                "Phase 6",
            ),
            (Box::new(|j| j.limits.memory_mb = 4096), "memory_mb"),
            (Box::new(|j| j.limits.pids = 0), "pids"),
            (Box::new(|j| j.steps.clear()), "steps"),
            (
                Box::new(|j| j.steps[0].name = "Bad Name".into()),
                "step name",
            ),
            (Box::new(|j| j.steps[0].cmd = "a\nb".into()), "one line"),
            (Box::new(|j| j.steps[0].timeout_secs = 5000), "wall time"),
            (
                Box::new(|j| j.services[0].name = "DB".into()),
                "service name",
            ),
            (
                Box::new(|j| {
                    j.services[0].env.insert("lower".into(), "x".into());
                }),
                "env",
            ),
            (
                Box::new(|j| j.services[0].data = Some("../x".into())),
                "data path",
            ),
            (
                Box::new(|j| j.services.push(j.services[0].clone())),
                "twice",
            ),
        ];
        for (change, want) in cases {
            let mut j = job();
            change(&mut j);
            let err = validate(&j, &yes).unwrap_err();
            assert!(err.contains(want), "{want}: {err}");
        }
        assert!(validate(&job(), &|i: &str| i != IMG)
            .unwrap_err()
            .contains("allowlist"));
        assert!(image_ref("/abs@sha256:00").is_err());
        assert!(job_id("j-0123456789abcdef").is_ok() && job_id("x-01234567").is_err());
    }

    #[test]
    fn results_are_checked_against_the_job_sent() {
        let j = job();
        let ok = JobResult {
            job: j.job.clone(),
            nonce: j.nonce.clone(),
            steps: vec![StepResult {
                name: "test".into(),
                exit: 0,
                timed_out: false,
                oom: false,
                tail: "ok".into(),
            }],
            patch: None,
            cleanup: Cleanup::default(),
            error: None,
        };
        let line = |r: &JobResult| serde_json::to_string(r).unwrap();
        match check_result(&line(&ok), &j).unwrap() {
            Polled::Done(r) => assert!(r.passed()),
            p => panic!("{p:?}"),
        }
        assert_eq!(
            check_result(r#"{"job":"j-7c1e2a9b","pending":true}"#, &j),
            Ok(Polled::Pending)
        );
        assert!(check_result(r#"{"job":"j-00000000","pending":true}"#, &j).is_err());
        let bad: Vec<(JobResult, &str)> = vec![
            (
                JobResult {
                    nonce: "other".into(),
                    ..ok.clone()
                },
                "nonce",
            ),
            (
                JobResult {
                    steps: vec![],
                    ..ok.clone()
                },
                "do not match",
            ),
            (
                JobResult {
                    patch: Some("AA==".into()),
                    ..ok.clone()
                },
                "did not ask",
            ),
            (
                JobResult {
                    steps: vec![StepResult {
                        tail: "x\n".repeat(41),
                        ..ok.steps[0].clone()
                    }],
                    ..ok.clone()
                },
                "40 lines",
            ),
        ];
        for (r, want) in bad {
            assert!(
                check_result(&line(&r), &j).unwrap_err().contains(want),
                "{want}"
            );
        }
        let early = JobResult {
            steps: vec![],
            error: Some("podman failed".into()),
            ..ok.clone()
        };
        assert!(matches!(check_result(&line(&early), &j), Ok(Polled::Done(r)) if !r.passed()));
        assert!(check_result("not json", &j).is_err());
        assert!(check_result(&"x".repeat(MAX_RESULT + 1), &j)
            .unwrap_err()
            .contains("256 KB"));
        assert!(check_result(r#"{"job":"j-7c1e2a9b","extra":1}"#, &j).is_err());
    }

    #[test]
    fn tails_keep_40_lines_and_8_kb() {
        let many: String = (0..100).map(|i| format!("{i}\n")).collect();
        assert_eq!(tail(&many).lines().count(), 40);
        let wide = "é".repeat(10_000);
        let t = tail(&wide);
        assert!(t.len() <= MAX_TAIL && t.chars().all(|c| c == 'é'));
    }
}
