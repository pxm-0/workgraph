//! Resume (design §7.4): the answers an interrupted run already got are fed again instead of
//! calling `claude` twice. Only a run of the same binary and assets replays; its raw envelopes come
//! from its own log under the agent-denied `~/.workgraph/runs/`, and each is parsed again by the
//! same validators, so a replayed answer is trusted exactly as much as a live one. A run whose guard
//! log holds an incident never replays.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One recorded answer: its label, the provider that gave it, and the raw stdout.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorded {
    pub label: String,
    pub provider: String,
    pub stdout: String,
}

#[derive(Debug, Default)]
pub struct Replay {
    pub from: Option<PathBuf>,
    left: Mutex<Vec<Recorded>>,
}

/// The binary's version stamp: sha256 of its own file, 12 hex digits.
pub fn binary_hash() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| crate::haruspex::cli::sha256_file(&p).ok())
        .map(|h| h[..12].to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// What the run journal says about versions: `(binary, assets)` of its `run_started`.
pub fn versions(journal: &[Value]) -> Option<(String, String)> {
    let s = journal.iter().find(|e| e["kind"] == "run_started")?;
    Some((
        s["binary"].as_str()?.to_string(),
        s["assets"].as_str()?.to_string(),
    ))
}

impl Replay {
    pub fn none() -> Replay {
        Replay::default()
    }

    /// The answers `run_dir` recorded, if its binary and assets equal `(binary, assets)`.
    pub fn load(run_dir: &Path, binary: &str, assets: &str) -> Result<Replay, String> {
        let journal = crate::journal::read(&run_dir.join("journal.jsonl"))?;
        match versions(&journal) {
            Some((b, a)) if b == binary && a == assets => {}
            Some((b, a)) => {
                return Err(format!(
                    "version pin: the run used binary {b} and assets {a}, this is {binary} and {assets}"
                ))
            }
            None => return Err("the run journal has no run_started".into()),
        }
        let guard = std::fs::read_to_string(run_dir.join("guard.jsonl")).unwrap_or_default();
        if guard
            .lines()
            .any(|l| l.contains("\"decision\":\"incident\""))
        {
            return Err("the run had a guard incident; it is never replayed".into());
        }
        let mut left = Vec::new();
        for e in journal
            .iter()
            .filter(|e| e["kind"] == "llm_finished" && e["outcome"] == "ok")
        {
            let (Some(label), Some(seq), Some(provider)) = (
                e["label"].as_str(),
                e["call_seq"].as_u64(),
                e["provider"].as_str(),
            ) else {
                continue;
            };
            let path = run_dir
                .join("logs")
                .join(format!("{}.json", super::runner::file_stem(seq, label)));
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let log: Value = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
            if log["label"] != label {
                return Err(format!("{} does not hold {label}", path.display()));
            }
            left.push(Recorded {
                label: label.into(),
                provider: provider.into(),
                stdout: log["stdout"].as_str().unwrap_or_default().into(),
            });
        }
        Ok(Replay {
            from: Some(run_dir.to_path_buf()),
            left: Mutex::new(left),
        })
    }

    /// The next recorded answer for `label`, removed so it is used once.
    pub fn take(&self, label: &str) -> Option<Recorded> {
        let mut g = self.left.lock().ok()?;
        let i = g.iter().position(|r| r.label == label)?;
        Some(g.remove(i))
    }

    pub fn len(&self) -> usize {
        self.left.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(name: &str, binary: &str, incident: bool) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wg-replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("logs")).unwrap();
        let j = crate::journal::Journal::open(&d.join("journal.jsonl"), "r-1").unwrap();
        j.append("run_started", json!({ "binary": binary, "assets": "a1" }))
            .unwrap();
        for (seq, label, outcome) in [(2, "read", "ok"), (3, "work:a", "Died"), (4, "read", "ok")] {
            j.append("llm_finished", json!({ "label": label, "call_seq": seq, "outcome": outcome, "provider": "subscription" })).unwrap();
            let stem = crate::claude::runner::file_stem(seq, label);
            std::fs::write(
                d.join("logs").join(format!("{stem}.json")),
                json!({ "label": label, "stdout": format!("out{seq}") }).to_string(),
            )
            .unwrap();
        }
        if incident {
            std::fs::write(d.join("guard.jsonl"), "{\"decision\":\"incident\"}\n").unwrap();
        }
        d
    }

    #[test]
    fn ok_answers_replay_once_each_in_order() {
        let r = Replay::load(&run("ok", "b1", false), "b1", "a1").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r.take("read").unwrap().stdout, "out2");
        assert_eq!(r.take("read").unwrap().stdout, "out4");
        assert!(r.take("read").is_none() && r.take("work:a").is_none());
        assert!(r.is_empty() && Replay::none().is_empty());
    }

    #[test]
    fn another_version_or_an_incident_never_replays() {
        assert!(Replay::load(&run("v", "b1", false), "b2", "a1")
            .unwrap_err()
            .contains("version pin"));
        assert!(Replay::load(&run("v2", "b1", false), "b1", "a2")
            .unwrap_err()
            .contains("version pin"));
        assert!(Replay::load(&run("inc", "b1", true), "b1", "a1")
            .unwrap_err()
            .contains("incident"));
        assert!(Replay::load(Path::new("/nonexistent"), "b", "a").is_err());
        assert_eq!(binary_hash().len(), 12);
    }
}
