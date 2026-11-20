//! The self-ledger (design §12.1-§12.3): append-only JSONL under `~/.workgraph`, each line carrying
//! the sha256 of the line before it, so an edit or a deletion anywhere breaks the chain and the
//! gate refuses to decide on it. It records what the factory authored (head SHAs and their runs),
//! every gate decision, merges with their merge SHA, proposals filed, eval spend, installs and
//! rollbacks.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub struct Ledger {
    path: PathBuf,
}

fn sha(line: &str) -> String {
    format!("{:x}", Sha256::digest(line.as_bytes()))
}

/// An exclusive `flock` on `<path>.lock`, held until the file drops: the owner's CLI commands and a
/// shift's self lane never interleave a read-modify-write of the ledger or of `self.json`.
pub fn exclusive(path: &Path) -> Result<std::fs::File, String> {
    if let Some(d) = path.parent() {
        crate::state_dirs::private_dir(d)?;
    }
    let lock = path.with_extension("lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock)
        .map_err(|e| format!("cannot open {}: {e}", lock.display()))?;
    f.lock()
        .map_err(|e| format!("cannot lock {}: {e}", lock.display()))?;
    Ok(f)
}

impl Ledger {
    pub fn at(path: &Path) -> Ledger {
        Ledger {
            path: path.to_path_buf(),
        }
    }

    /// Every entry, oldest first, after checking the hash chain end to end.
    pub fn entries(&self) -> Result<Vec<Value>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(format!("cannot read {}: {e}", self.path.display())),
        };
        let mut prev = GENESIS.to_string();
        let mut out = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let v: Value = serde_json::from_str(line)
                .map_err(|e| format!("self-ledger line {}: not JSON: {e}", n + 1))?;
            if v["prev"].as_str() != Some(prev.as_str()) || v["seq"].as_u64() != Some(n as u64 + 1)
            {
                return Err(format!(
                    "self-ledger line {}: the hash chain is broken; the ledger was edited",
                    n + 1
                ));
            }
            prev = sha(line);
            out.push(v);
        }
        Ok(out)
    }

    /// Appends `{seq, at, prev, kind, ...fields}` (fsynced) and returns it.
    pub fn append(&self, kind: &str, fields: Value) -> Result<Value, String> {
        let _held = exclusive(&self.path)?;
        let entries = self.entries()?;
        let prev = match std::fs::read_to_string(&self.path) {
            Ok(t) => t.lines().last().map(sha).unwrap_or_else(|| GENESIS.into()),
            Err(_) => GENESIS.into(),
        };
        let mut m = Map::new();
        m.insert("seq".into(), json!(entries.len() as u64 + 1));
        m.insert("at".into(), json!(crate::journal::now()));
        m.insert("prev".into(), json!(prev));
        m.insert("kind".into(), json!(kind));
        if let Value::Object(f) = fields {
            for (k, v) in f {
                if !m.contains_key(&k) {
                    m.insert(k, v);
                }
            }
        }
        let v = Value::Object(m);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("cannot open {}: {e}", self.path.display()))?;
        writeln!(f, "{v}")
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))?;
        Ok(v)
    }

    pub fn of_kind(&self, kind: &str) -> Result<Vec<Value>, String> {
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| e["kind"] == kind)
            .collect())
    }

    /// The run that authored head `sha`, if the factory produced it.
    pub fn authored_by(&self, sha: &str) -> Result<Option<String>, String> {
        Ok(self
            .of_kind("authored")?
            .iter()
            .find(|e| e["head"] == sha)
            .and_then(|e| e["run"].as_str().map(str::to_string)))
    }

    /// The last gate decision recorded for `pr` at `head`.
    pub fn last_decision(&self, pr: u64, head: &str) -> Result<Option<Value>, String> {
        Ok(self
            .of_kind("decision")?
            .into_iter()
            .rev()
            .find(|d| d["pr"] == pr && d["head"] == head))
    }

    /// Every factory merge, oldest first. A `merging` decision counts from the moment it is recorded
    /// (so a merge whose follow-up call failed still holds every rate limit), until a `merged`
    /// entry names its merge SHA or a later decision on the same head says it never happened; until
    /// then its `merge` is null.
    pub fn merges(&self) -> Result<Vec<Value>, String> {
        let all = self.entries()?;
        let same = |a: &Value, b: &Value| a["pr"] == b["pr"] && a["head"] == b["head"];
        let mut out: Vec<Value> = Vec::new();
        for (i, e) in all.iter().enumerate() {
            let unresolved = e["kind"] == "decision"
                && e["outcome"] == "merging"
                && !all[i + 1..]
                    .iter()
                    .any(|l| (l["kind"] == "merged" || l["kind"] == "decision") && same(l, e));
            if e["kind"] == "merged" {
                out.push(e.clone());
            } else if unresolved {
                out.push(
                    json!({ "seq": e["seq"], "at": e["at"], "kind": "merged", "pr": e["pr"],
                                 "head": e["head"], "merge": null, "decision": e["seq"] }),
                );
            }
        }
        Ok(out)
    }

    /// `merging` decisions with no merge SHA yet, as `(pr, head, decision seq)`.
    pub fn unresolved_merges(&self) -> Result<Vec<(u64, String, u64)>, String> {
        Ok(self
            .merges()?
            .iter()
            .filter(|m| m["merge"].is_null())
            .filter_map(|m| {
                Some((
                    m["pr"].as_u64()?,
                    m["head"].as_str()?.to_string(),
                    m["decision"].as_u64()?,
                ))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_detects_any_edit_and_lookups_work() {
        let dir = std::env::temp_dir().join(format!("wg-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let l = Ledger::at(&dir.join("self-ledger.jsonl"));
        assert!(l.entries().unwrap().is_empty());
        l.append(
            "authored",
            json!({ "pr": 7, "head": "abc", "run": "r-1", "seq": 99 }),
        )
        .unwrap();
        l.append("merged", json!({ "pr": 7, "head": "abc", "merge": "def" }))
            .unwrap();
        let all = l.entries().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["seq"], 1, "a field never overrides the chain's own");
        assert_eq!(l.authored_by("abc").unwrap().as_deref(), Some("r-1"));
        assert_eq!(l.authored_by("zzz").unwrap(), None);
        assert_eq!(l.merges().unwrap()[0]["merge"], "def");
        let text = std::fs::read_to_string(dir.join("self-ledger.jsonl")).unwrap();
        std::fs::write(
            dir.join("self-ledger.jsonl"),
            text.replace("\"r-1\"", "\"r-2\""),
        )
        .unwrap();
        assert!(l.entries().unwrap_err().contains("chain is broken"));
        assert!(
            l.append("x", json!({})).is_err(),
            "never appends to a broken chain"
        );
        let first = text.lines().next().unwrap();
        std::fs::write(dir.join("self-ledger.jsonl"), format!("{first}\n{first}\n")).unwrap();
        assert!(l.entries().is_err(), "a replayed line breaks it too");
        std::fs::write(dir.join("self-ledger.jsonl"), "nope\n").unwrap();
        assert!(l.entries().unwrap_err().contains("not JSON"));
    }

    #[test]
    fn concurrent_writers_never_break_the_chain() {
        let dir = std::env::temp_dir().join(format!("wg-ledger-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("self-ledger.jsonl");
        let writers: Vec<_> = (0..8)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..15 {
                        Ledger::at(&path)
                            .append("x", json!({ "w": w, "i": i }))
                            .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(Ledger::at(&path).entries().unwrap().len(), 120);
    }

    #[test]
    fn a_merging_decision_counts_as_a_merge_until_resolved() {
        let dir = std::env::temp_dir().join(format!("wg-ledger-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let l = Ledger::at(&dir.join("self-ledger.jsonl"));
        let merging = |pr: u64| json!({ "pr": pr, "head": "h", "outcome": "merging" });
        l.append("decision", merging(1)).unwrap();
        let m = l.merges().unwrap();
        assert_eq!((m.len(), m[0]["merge"].is_null()), (1, true), "{m:?}");
        assert_eq!(l.unresolved_merges().unwrap(), [(1, "h".to_string(), 1)]);
        l.append("merged", json!({ "pr": 1, "head": "h", "merge": "m1" }))
            .unwrap();
        assert_eq!(l.merges().unwrap().len(), 1, "one merge, now with its SHA");
        assert_eq!(l.merges().unwrap()[0]["merge"], "m1");
        // A merge that never happened is followed by a refusal on the same head.
        l.append("decision", merging(2)).unwrap();
        l.append(
            "decision",
            json!({ "pr": 2, "head": "h", "outcome": "refused" }),
        )
        .unwrap();
        assert_eq!(l.merges().unwrap().len(), 1);
        assert!(l.unresolved_merges().unwrap().is_empty());
        assert_eq!(
            l.last_decision(2, "h").unwrap().unwrap()["outcome"],
            "refused"
        );
        assert!(l.last_decision(3, "h").unwrap().is_none());
    }
}
