//! `journal.jsonl`: one JSON object per command and event, fsynced per line, with a monotonic
//! `seq` (design §13.2). Reports are built from it.

use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Unix seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Nanoseconds since the epoch, for unique temporary names.
pub fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub struct Journal {
    path: PathBuf,
    run: String,
    inner: Mutex<(File, u64)>,
}

impl Journal {
    pub fn open(path: &Path, run: &str) -> Result<Journal, String> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("cannot open the journal {}: {e}", path.display()))?;
        let seq = std::fs::read_to_string(path)
            .map(|t| t.lines().count() as u64)
            .unwrap_or(0);
        Ok(Journal {
            path: path.to_path_buf(),
            run: run.to_string(),
            inner: Mutex::new((f, seq)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `{seq, ts, run, kind, ...fields}` and returns its `seq`.
    pub fn append(&self, kind: &str, fields: Value) -> Result<u64, String> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| "the journal lock is poisoned".to_string())?;
        g.1 += 1;
        let seq = g.1;
        let mut obj = Map::new();
        obj.insert("seq".into(), json!(seq));
        obj.insert("ts".into(), json!(now()));
        obj.insert("run".into(), json!(self.run));
        obj.insert("kind".into(), json!(kind));
        if let Value::Object(extra) = fields {
            obj.extend(extra);
        }
        let line = Value::Object(obj).to_string();
        writeln!(g.0, "{line}").map_err(|e| format!("cannot write the journal: {e}"))?;
        g.0.sync_data()
            .map_err(|e| format!("cannot sync the journal: {e}"))?;
        Ok(seq)
    }
}

/// Every line of a journal file, parsed.
pub fn read(path: &Path) -> Result<Vec<Value>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    text.lines()
        .map(|l| serde_json::from_str(l).map_err(|e| format!("bad journal line: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_numbered_lines_and_continues_after_reopen() {
        let dir = std::env::temp_dir().join(format!("wg-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("journal.jsonl");
        let j = Journal::open(&p, "r-1").unwrap();
        assert_eq!(j.append("run_started", json!({ "x": 1 })).unwrap(), 1);
        assert_eq!(j.append("warning", json!({})).unwrap(), 2);
        drop(j);
        let j = Journal::open(&p, "r-1").unwrap();
        assert_eq!(j.append("step", json!({})).unwrap(), 3);
        let lines = read(&p).unwrap();
        assert_eq!(lines[0]["kind"], "run_started");
        assert_eq!(lines[0]["x"], 1);
        assert_eq!(lines[2]["seq"], 3);
        assert_eq!(j.path(), p.as_path());
    }
}
