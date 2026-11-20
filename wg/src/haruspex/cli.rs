//! The pinned Haruspex CLI (design §9.1). Its path comes only from `factory.toml [haruspex]`, must
//! be absolute and outside the repo, the run dir and `~/.workgraph-work/`, and its sha256 is
//! re-checked before every call, so a rewritten file is an incident, not a different gate.

use crate::config::Pin;
use crate::ports::Haruspex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// The canonical CLI path when it is absolute and outside every `forbidden` root.
pub fn check_location(cli: &str, forbidden: &[PathBuf]) -> Result<PathBuf, String> {
    if !cli.starts_with('/') {
        return Err("the Haruspex CLI path must be absolute".into());
    }
    let canon = Path::new(cli)
        .canonicalize()
        .map_err(|e| format!("the Haruspex CLI {cli} is not readable: {e}"))?;
    for root in forbidden {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        if canon.starts_with(&root) {
            return Err(format!(
                "the Haruspex CLI {} lies inside {}; pin one outside every repo and run dir",
                canon.display(),
                root.display()
            ));
        }
    }
    Ok(canon)
}

pub struct PinnedHaruspex {
    pub cli: PathBuf,
    pub sha256: String,
    pub repo: PathBuf,
}

impl PinnedHaruspex {
    /// Validates the pin against the repo and run dirs, then checks the hash once.
    pub fn new(pin: &Pin, repo: &Path, forbidden: &[PathBuf]) -> Result<PinnedHaruspex, String> {
        let mut roots = forbidden.to_vec();
        roots.push(repo.to_path_buf());
        let cli = check_location(&pin.cli, &roots)?;
        let h = PinnedHaruspex {
            cli,
            sha256: pin.sha256.clone(),
            repo: repo.to_path_buf(),
        };
        h.verify()?;
        Ok(h)
    }

    pub fn verify(&self) -> Result<(), String> {
        let got = sha256_file(&self.cli)?;
        if got != self.sha256 {
            return Err(format!(
                "incident: the Haruspex CLI {} changed since it was pinned (sha256 {got})",
                self.cli.display()
            ));
        }
        Ok(())
    }

    /// `python3 <cli> <args>` in the repo with the scrubbed environment, after the hash check.
    pub fn run(&self, args: &[&str]) -> Result<(i32, String), String> {
        self.verify()?;
        let out = Command::new("python3")
            .arg(&self.cli)
            .args(args)
            .current_dir(&self.repo)
            .env_clear()
            .envs(crate::exec::env::scrubbed())
            .output()
            .map_err(|e| format!("cannot run Haruspex: {e}"))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok((out.status.code().unwrap_or(-1), text))
    }

    fn read_json(&self, rel: &str) -> Result<Value, String> {
        let p = self.repo.join(rel);
        let text = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {rel}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{rel} is not valid JSON: {e}"))
    }
}

impl Haruspex for PinnedHaruspex {
    fn gate(&self, gate: &str) -> Result<Vec<String>, String> {
        if !["ready-to-build", "ready-to-release"].contains(&gate) {
            return Err(format!("the kernel evaluates no gate {gate:?}"));
        }
        self.verify()?;
        let out = Command::new("python3")
            .arg(&self.cli)
            .args(["gate", gate, ".", "--json"])
            .current_dir(&self.repo)
            .env_clear()
            .envs(crate::exec::env::scrubbed())
            .output()
            .map_err(|e| format!("cannot run Haruspex: {e}"))?;
        let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
            format!(
                "the {gate} gate printed no JSON: {}",
                crate::exec::run::tail(&String::from_utf8_lossy(&out.stderr))
            )
        })?;
        v["blockers"]
            .as_array()
            .ok_or("the gate JSON has no blockers list")?
            .iter()
            .map(|b| {
                b.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "a blocker is not a string".to_string())
            })
            .collect()
    }

    fn task(&self, id: &str) -> Result<Value, String> {
        crate::ids::task_id(id)?;
        self.read_json(&format!(".haruspex/tasks/{id}.json"))
    }

    fn approvals(&self) -> Result<Value, String> {
        match self.read_json(".haruspex/state.json") {
            Ok(v) => Ok(v["approvals"].clone()),
            Err(_) if !self.repo.join(".haruspex/state.json").exists() => Ok(Value::Null),
            Err(e) => Err(e),
        }
    }

    fn project_commands(&self) -> Result<BTreeMap<String, String>, String> {
        let v = self.read_json(".haruspex/project.json")?;
        Ok(v["commands"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, c)| c.as_str().map(|c| (k.clone(), c.to_string())))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn managed_hash(&self) -> Result<String, String> {
        tree_hash(
            &self.repo.join(".haruspex"),
            &["bin", "templates", "managed"],
        )
    }
}

/// sha256 over the sorted relative paths and contents of `dirs` under `base`.
pub fn tree_hash(base: &Path, dirs: &[&str]) -> Result<String, String> {
    let mut files = Vec::new();
    for d in dirs {
        collect(&base.join(d), &mut files)?;
    }
    if files.is_empty() {
        return Ok("absent".into());
    }
    files.sort();
    let mut h = Sha256::new();
    for f in files {
        h.update(
            f.strip_prefix(base)
                .unwrap_or(&f)
                .to_string_lossy()
                .as_bytes(),
        );
        h.update([0]);
        h.update(std::fs::read(&f).map_err(|e| format!("cannot read {}: {e}", f.display()))?);
        h.update([0]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
    };
    for e in entries {
        let p = e
            .map_err(|e| format!("cannot list {}: {e}", dir.display()))?
            .path();
        if p.is_dir() {
            collect(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_and_hash_checks() {
        let base = std::env::temp_dir().join(format!("wg-hx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.haruspex/managed")).unwrap();
        std::fs::create_dir_all(base.join("tools")).unwrap();
        let cli = base.join("tools/haruspex.py");
        std::fs::write(&cli, "print('hi')").unwrap();
        let inside = base.join("repo/haruspex.py");
        std::fs::write(&inside, "x").unwrap();
        let repo = base.join("repo");
        assert!(check_location("rel.py", &[]).is_err());
        assert!(
            check_location(&inside.to_string_lossy(), std::slice::from_ref(&repo))
                .unwrap_err()
                .contains("lies inside")
        );
        let sha = sha256_file(&cli).unwrap();
        let pin = Pin {
            cli: cli.to_string_lossy().into(),
            sha256: sha.clone(),
        };
        let h = PinnedHaruspex::new(&pin, &repo, &[]).unwrap();
        std::fs::write(&cli, "print('tampered')").unwrap();
        assert!(h.verify().unwrap_err().contains("incident"));
        assert_eq!(
            tree_hash(&repo.join(".haruspex"), &["managed"]).unwrap(),
            "absent"
        );
        std::fs::write(repo.join(".haruspex/managed/gates.json"), "{}").unwrap();
        let a = tree_hash(&repo.join(".haruspex"), &["managed"]).unwrap();
        std::fs::write(repo.join(".haruspex/managed/gates.json"), "{ }").unwrap();
        assert_ne!(a, tree_hash(&repo.join(".haruspex"), &["managed"]).unwrap());
        assert_eq!(h.approvals().unwrap(), Value::Null, "no state.json yet");
    }
}
