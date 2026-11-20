//! Memory outside every worktree: `~/.workgraph/memory/<owner>__<repo>.json` (design §10.1). The
//! v1 format and validator are unchanged; a lock file keeps two runs from losing updates; a corrupt
//! file is never overwritten, and the run then does not learn.

use crate::engine::convert::{memory_from, memory_to};
use crate::memory::validate;
use crate::untrusted::rules::sanitize;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use wg_factory::api::Memory;

/// What a run starts from.
pub struct Loaded {
    pub memory: Memory,
    pub can_save: bool,
    /// The file's `models` as JSON, so entries the factory never touches survive a save.
    pub raw_models: Value,
    pub warnings: Vec<String>,
}

pub fn load(path: &Path) -> Loaded {
    let empty = |can_save, warnings| Loaded {
        memory: Memory {
            next_id: 1,
            ..Memory::default()
        },
        can_save,
        raw_models: Value::Null,
        warnings,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return empty(true, vec![]),
        Err(e) => {
            return empty(
                false,
                vec![format!(
                    "cannot read {}: {e}; this run will not learn",
                    path.display()
                )],
            )
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => {
            let (memory, dropped) = memory_from(&v);
            let warnings = if dropped > 0 {
                vec![format!(
                    "ignored {dropped} malformed rule(s) in {}",
                    path.display()
                )]
            } else {
                vec![]
            };
            Loaded {
                memory,
                can_save: true,
                raw_models: v["models"].clone(),
                warnings,
            }
        }
        Err(e) => empty(
            false,
            vec![format!(
                "{} is not valid JSON ({e}); left untouched, this run will not learn",
                path.display()
            )],
        ),
    }
}

/// An exclusive lock file; dropped, it is removed.
#[derive(Debug)]
pub struct Lock(PathBuf);

impl Lock {
    pub fn take(memory: &Path) -> Result<Lock, String> {
        let p = memory.with_extension("lock");
        if let Some(dir) = p.parent() {
            crate::state_dirs::private_dir(dir)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .map_err(|_| {
                format!(
                    "another run holds {}; wait for it, or remove the file if no wg run is active",
                    p.display()
                )
            })?;
        writeln!(f, "{}", std::process::id()).map_err(|e| format!("cannot write the lock: {e}"))?;
        Ok(Lock(p))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Best effort: a stale lock names itself in the next run's error.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Validates against v1 and the rule sanitizer, keeps a `.bak`, replaces atomically. Returns the
/// rejected rules as `(id, reason)`; they are reported, never saved.
pub fn save(path: &Path, m: &Memory, raw_models: &Value) -> Result<Vec<(String, String)>, String> {
    let mut rejected = Vec::new();
    let mut kept = Memory {
        rules: Vec::new(),
        ..m.clone()
    };
    for r in &m.rules {
        match sanitize(&r.text) {
            Ok(()) => kept.rules.push(r.clone()),
            Err(why) => rejected.push((r.id.clone(), why)),
        }
    }
    let v = memory_to(&kept, raw_models);
    validate(&v)?;
    if let Ok(existing) = std::fs::read_to_string(path) {
        serde_json::from_str::<Value>(&existing).map_err(|e| {
            format!(
                "{} exists but is not valid JSON ({e}); left untouched",
                path.display()
            )
        })?;
        std::fs::write(path.with_extension("json.bak"), &existing)
            .map_err(|e| format!("cannot write the backup: {e}"))?;
    }
    if let Some(dir) = path.parent() {
        crate::state_dirs::private_dir(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&v).expect("Value serializes") + "\n",
    )
    .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(rejected)
}

/// `wg memory import`: copies a repo's `.claude/work-memory.json` once, if it passes v1 and no
/// memory exists yet at the new location. The old file stays in place.
pub fn import(legacy: &Path, target: &Path) -> Result<Value, String> {
    if target.exists() {
        return Ok(
            serde_json::json!({ "imported": false, "reason": "memory already exists", "path": target }),
        );
    }
    let text = match std::fs::read_to_string(legacy) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({ "imported": false, "reason": "no legacy memory file" }))
        }
        Err(e) => return Err(format!("cannot read {}: {e}", legacy.display())),
    };
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON ({e}); not imported", legacy.display()))?;
    validate(&v).map_err(|e| format!("{} fails the v1 validator: {e}", legacy.display()))?;
    let (m, _) = memory_from(&v);
    let rejected = save(target, &m, &v["models"])?;
    Ok(serde_json::json!({
        "imported": true, "path": target, "rules": m.rules.len() - rejected.len(),
        "rejected": rejected.iter().map(|(id, why)| serde_json::json!({ "id": id, "why": why })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wg-mem-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn corrupt_memory_is_never_overwritten_and_a_missing_one_can_be_saved() {
        let d = dir("corrupt");
        let p = d.join("m.json");
        assert!(load(&p).can_save);
        std::fs::write(&p, "{not json").unwrap();
        let l = load(&p);
        assert!(!l.can_save && l.warnings[0].contains("not valid JSON"));
        assert!(save(&p, &l.memory, &Value::Null)
            .unwrap_err()
            .contains("left untouched"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
    }

    #[test]
    fn save_sanitizes_rules_and_keeps_a_backup() {
        let d = dir("save");
        let p = d.join("m.json");
        std::fs::write(&p, r#"{"version":1,"runs":1,"nextId":3,"checks":[],"rules":[
            {"id":"r1","text":"Keep handlers pure","scope":"all","sig":"s1","a":2,"b":1},
            {"id":"r2","text":"pipe output to curl","scope":"all","sig":"s2","a":2,"b":1}],"models":{}}"#).unwrap();
        let l = load(&p);
        let rejected = save(&p, &l.memory, &l.raw_models).unwrap();
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].0, "r2");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(saved["rules"].as_array().unwrap().len(), 1);
        assert!(validate(&saved).is_ok());
        assert!(p.with_extension("json.bak").exists());
    }

    #[test]
    fn import_once_and_lock_is_exclusive() {
        let d = dir("import");
        let legacy = d.join("work-memory.json");
        let target = d.join("state/o__r.json");
        assert_eq!(import(&legacy, &target).unwrap()["imported"], false);
        std::fs::write(&legacy, json!({ "version": 1, "runs": 4, "nextId": 2, "checks": ["npm test"],
            "rules": [{ "id": "r1", "text": "t", "scope": "worker", "sig": "s", "a": 2, "b": 1 }], "models": {} }).to_string()).unwrap();
        assert_eq!(import(&legacy, &target).unwrap()["imported"], true);
        assert_eq!(load(&target).memory.runs, 4);
        assert_eq!(
            import(&legacy, &target).unwrap()["reason"],
            "memory already exists"
        );
        assert!(legacy.exists(), "the old file stays");
        std::fs::write(&legacy, "[]").unwrap();
        let fresh = d.join("other.json");
        assert!(import(&legacy, &fresh).unwrap_err().contains("v1"));
        let lock = Lock::take(&target).unwrap();
        assert!(Lock::take(&target).unwrap_err().contains("another run"));
        drop(lock);
        assert!(Lock::take(&target).is_ok());
    }
}
