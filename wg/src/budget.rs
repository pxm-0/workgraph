//! The append-only cost ledger, `~/.workgraph/cost.jsonl`, and the per-task call cap (design
//! §5.2, §5.3, D3). There is no dollar cap: every call is recorded with its provider and model id,
//! and a missing cost is `"unknown"`, never zero.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The ledger entry every call needs; only `Ledger::lease` makes one.
#[derive(Debug)]
pub struct BudgetLease {
    id: u64,
    label: String,
    provider: String,
    model_id: String,
}

impl BudgetLease {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// One call's settled figures.
#[derive(Debug, Default, Clone)]
pub struct Spend {
    pub tier: String,
    pub stage: String,
    pub task: String,
    pub cost_micros: Option<u64>,
    pub model_used: Option<String>,
    pub turns: Option<u64>,
    pub session_id: Option<String>,
    pub duration_ms: u64,
    pub outcome: String,
    /// The failure that moved this call to another provider, on the first call after a switch.
    pub switched_after: Option<String>,
}

pub struct Ledger {
    path: PathBuf,
    run: String,
    max_calls: u32,
    state: Mutex<(u32, u64)>,
}

impl Ledger {
    pub fn new(path: &Path, run: &str, max_calls: u32) -> Ledger {
        Ledger {
            path: path.to_path_buf(),
            run: run.to_string(),
            max_calls,
            state: Mutex::new((0, 0)),
        }
    }

    /// A lease for one call, or `Err` when the task's call cap is reached.
    pub fn lease(
        &self,
        label: &str,
        provider: &str,
        model_id: &str,
    ) -> Result<BudgetLease, String> {
        let mut g = self
            .state
            .lock()
            .map_err(|_| "the ledger lock is poisoned".to_string())?;
        if g.0 >= self.max_calls {
            return Err(format!("call cap of {} reached", self.max_calls));
        }
        g.0 += 1;
        g.1 += 1;
        Ok(BudgetLease {
            id: g.1,
            label: label.to_string(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })
    }

    pub fn calls(&self) -> u32 {
        self.state.lock().map(|g| g.0).unwrap_or(u32::MAX)
    }

    /// Settles a lease: one ledger line. A gateway's figure is Claude Code's estimate at Anthropic
    /// prices, not the bill, so it is recorded as unknown (design §5.3 step 3).
    pub fn settle(&self, lease: BudgetLease, s: &Spend, first_party: bool) -> Result<(), String> {
        let cost = match (first_party, s.cost_micros) {
            (true, Some(c)) => json!(c),
            _ => json!("unknown"),
        };
        let line = json!({
            "ts": crate::journal::now(),
            "run": self.run,
            "lease": lease.id,
            "label": lease.label,
            "stage": s.stage,
            "task": s.task,
            "tier": s.tier,
            "provider": lease.provider,
            "model_id": lease.model_id,
            "model_used": s.model_used,
            "cost_micros": cost,
            "turns": s.turns,
            "session_id": s.session_id,
            "duration_ms": s.duration_ms,
            "outcome": s.outcome,
            "switched_after": s.switched_after,
        });
        if let Some(dir) = self.path.parent() {
            crate::state_dirs::private_dir(dir)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("cannot open the cost ledger: {e}"))?;
        writeln!(f, "{line}").map_err(|e| format!("cannot write the cost ledger: {e}"))
    }
}

/// Spend of one run from the ledger: known micros, and how many calls had no known cost.
pub fn run_spend(path: &Path, run: &str) -> Result<(u64, u64, Vec<Value>), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0, vec![])),
        Err(e) => return Err(format!("cannot read the cost ledger: {e}")),
    };
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["run"] == run)
        .collect();
    let known: u64 = lines.iter().filter_map(|v| v["cost_micros"].as_u64()).sum();
    let unknown = lines.iter().filter(|v| !v["cost_micros"].is_u64()).count() as u64;
    Ok((known, unknown, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_respect_the_cap_and_unknown_cost_is_never_zero() {
        let dir = std::env::temp_dir().join(format!("wg-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cost.jsonl");
        let l = Ledger::new(&path, "r-1", 2);
        let a = l.lease("read", "subscription", "opus").unwrap();
        assert_eq!((a.provider(), a.model_id()), ("subscription", "opus"));
        let b = l.lease("challenge", "subscription", "sonnet").unwrap();
        assert!(l
            .lease("work:a", "subscription", "haiku")
            .unwrap_err()
            .contains("call cap"));
        assert_eq!(l.calls(), 2);
        let spend = |c| Spend {
            cost_micros: c,
            ..Spend::default()
        };
        l.settle(a, &spend(Some(1500)), true).unwrap();
        l.settle(b, &spend(None), true).unwrap();
        let g = Ledger::new(&path, "r-2", 5);
        let gw = g.lease("read", "openrouter", "x/y").unwrap();
        g.settle(gw, &spend(Some(99)), false).unwrap();
        assert_eq!(run_spend(&path, "r-1").unwrap().0, 1500);
        assert_eq!(run_spend(&path, "r-1").unwrap().1, 1);
        let (known, unknown, lines) = run_spend(&path, "r-2").unwrap();
        assert_eq!((known, unknown), (0, 1), "gateway cost is unknown");
        assert_eq!(lines[0]["provider"], "openrouter");
        assert_eq!(run_spend(&dir.join("none"), "r").unwrap().0, 0);
    }
}
