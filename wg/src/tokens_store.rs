//! Tokens that outlive the process (`wg finalize --run`): `<run>/tokens.jsonl`, each line MAC'd
//! with the 0600 key `~/.workgraph/key`. Reloaded only by `finalize_run`, which re-checks the tree.

use crate::haruspex::contract::Contract;
use crate::tokens::{CheckResult, ReviewPass, SandboxRun};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

/// What finalize needs to know about the run, beyond the tokens.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunMeta {
    pub run: String,
    pub repo: String,
    pub task_id: String,
    pub gate: Vec<String>,
    /// The user's own changes, left out of the commit.
    pub exclude: Vec<String>,
    /// Check name → the approved `project` command `project.json` must still hold.
    pub pins: BTreeMap<String, String>,
    pub contract: Contract,
    pub approvals: Value,
    pub open_escalations: Vec<String>,
    pub decided: Vec<String>,
    /// A risky unit was built (or the repo is always tainted): checks and verify belong to the
    /// sandbox, never the Mac (design §8.1, §9.5).
    #[serde(default)]
    pub tainted: bool,
    /// Risky units and why, for the PR, the summary and the labels.
    #[serde(default)]
    pub risky: Vec<(String, String)>,
}

pub fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The MAC key, created on first use with mode 0600.
pub fn key(path: &Path) -> Result<Vec<u8>, String> {
    match std::fs::read(path) {
        Ok(k) if k.len() >= 32 => return Ok(k),
        Ok(_) => return Err(format!("{} is too short to be a key", path.display())),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("cannot read {}: {e}", path.display()))
        }
        Err(_) => {}
    }
    if let Some(dir) = path.parent() {
        crate::state_dirs::private_dir(dir)?;
    }
    let k = random_hex(32)?.into_bytes();
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    f.write_all(&k)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(k)
}

/// HMAC-SHA256 (RFC 2104).
pub fn mac(key: &[u8], msg: &[u8]) -> String {
    let mut k = if key.len() > 64 {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    k.resize(64, 0);
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    let outer = Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize();
    format!("{outer:x}")
}

fn line(key: &[u8], body: Value) -> String {
    let text = body.to_string();
    json!({ "body": text, "mac": mac(key, text.as_bytes()) }).to_string()
}

pub fn save(
    path: &Path,
    key: &[u8],
    meta: &RunMeta,
    checks: &[CheckResult],
    review: Option<&ReviewPass>,
    sandbox: &[SandboxRun],
) -> Result<(), String> {
    let mut lines = vec![line(key, json!({ "kind": "meta", "meta": meta }))];
    for c in checks {
        lines.push(line(
            key,
            json!({ "kind": "check", "name": c.name(), "command": c.command(), "exit": c.exit_code(),
                    "tail": c.output_tail(), "tree": c.tree(), "nonce": c.nonce(), "target": c.target().name() }),
        ));
    }
    for r in sandbox {
        lines.push(line(
            key,
            json!({ "kind": "sandbox", "job": r.job(), "nonce": r.nonce(), "label": r.label(), "tree": r.tree(),
                    "image": r.image(), "steps": r.steps(), "clean": r.clean() }),
        ));
    }
    if let Some(r) = review {
        lines.push(line(
            key,
            json!({ "kind": "review", "task": r.task(), "tree": r.tree(), "label": r.label() }),
        ));
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, lines.join("\n") + "\n")
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

pub type Loaded = (
    RunMeta,
    Vec<CheckResult>,
    Option<ReviewPass>,
    Vec<SandboxRun>,
);

/// Every line's MAC must verify, or nothing loads.
pub fn load(path: &Path, key: &[u8]) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("no tokens for this run ({}): {e}", path.display()))?;
    let mut meta = None;
    let mut checks = Vec::new();
    let mut review = None;
    let mut sandbox = Vec::new();
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    for l in text.lines() {
        let outer: Value =
            serde_json::from_str(l).map_err(|_| "a token line is not JSON".to_string())?;
        let body = outer["body"].as_str().ok_or("a token line has no body")?;
        if outer["mac"].as_str() != Some(mac(key, body.as_bytes()).as_str()) {
            return Err("incident: a token line fails its MAC".into());
        }
        let v: Value = serde_json::from_str(body).map_err(|_| "a token body is not JSON")?;
        match v["kind"].as_str() {
            Some("meta") => {
                meta = Some(
                    serde_json::from_value(v["meta"].clone())
                        .map_err(|e| format!("bad run meta: {e}"))?,
                )
            }
            Some("check") => {
                let make = if v["target"] == "sim_sandbox" {
                    CheckResult::in_sandbox
                } else {
                    CheckResult::new
                };
                checks.push(make(
                    &s(&v["name"]),
                    &s(&v["command"]),
                    v["exit"].as_i64().ok_or("a check token has no exit code")?,
                    &s(&v["tail"]),
                    &s(&v["tree"]),
                    &s(&v["nonce"]),
                ))
            }
            Some("sandbox") => {
                let steps: Vec<(String, i64)> = serde_json::from_value(v["steps"].clone())
                    .map_err(|e| format!("bad sandbox token: {e}"))?;
                sandbox.push(SandboxRun::restore(
                    &s(&v["job"]),
                    &s(&v["nonce"]),
                    &s(&v["label"]),
                    &s(&v["tree"]),
                    &s(&v["image"]),
                    steps,
                    v["clean"] == true,
                ))
            }
            Some("review") => {
                review = Some(ReviewPass::new(
                    &s(&v["task"]),
                    &s(&v["tree"]),
                    &s(&v["label"]),
                ))
            }
            _ => return Err("unknown token kind".into()),
        }
    }
    Ok((
        meta.ok_or("the tokens hold no run meta")?,
        checks,
        review,
        sandbox,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        assert_eq!(
            mac(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn round_trip_and_tamper_detection() {
        let dir = std::env::temp_dir().join(format!("wg-tokens-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let k = key(&dir.join("key")).unwrap();
        assert_eq!(key(&dir.join("key")).unwrap(), k, "the key is reused");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let meta = RunMeta {
            run: "r-1".into(),
            task_id: "T-1".into(),
            ..RunMeta::default()
        };
        let c = CheckResult::new("test", "npm test", 0, "ok", "tree1", "check:L1");
        let sc = CheckResult::in_sandbox("test", "npm test", 0, "ok", "tree1", "check:L2");
        let r = ReviewPass::new("T-1", "tree1", "review#1");
        let job = crate::sandbox::job::JobResult {
            job: "j-0000000a".into(),
            nonce: "n".into(),
            steps: vec![crate::sandbox::job::StepResult {
                name: "test".into(),
                exit: 0,
                timed_out: false,
                oom: false,
                tail: String::new(),
            }],
            patch: None,
            cleanup: Default::default(),
            error: None,
        };
        let sb = SandboxRun::new(&job, "check:L2", "tree1", "img@sha256:x");
        let p = dir.join("tokens.jsonl");
        save(
            &p,
            &k,
            &meta,
            &[c.clone(), sc.clone()],
            Some(&r),
            std::slice::from_ref(&sb),
        )
        .unwrap();
        let (m2, cs, rv, sbs) = load(&p, &k).unwrap();
        assert_eq!(
            (m2, cs, rv, sbs),
            (meta, vec![c, sc.clone()], Some(r), vec![sb])
        );
        assert_eq!(sc.target(), crate::tokens::Target::SimSandbox);
        let text = std::fs::read_to_string(&p)
            .unwrap()
            .replace("\\\"exit\\\":0", "\\\"exit\\\":1");
        std::fs::write(&p, text).unwrap();
        assert!(load(&p, &k).unwrap_err().contains("MAC"));
        assert!(load(&p, b"another key that is long enough!!").is_err());
    }
}
