//! Tier B, the live eval (design §12.3 step 5, §12.4): the base binary runs every arm, with base
//! assets and with the candidate's prompts and pipelines as an overlay, on the owner's fixtures from
//! the private store at the pinned commit. It is staged so the call count stays bounded, and runs
//! without a dollar ceiling (D3); every arm's spend goes to the self-ledger.
//!
//! - Stage 1: every fixture, both arms. A fixture whose escalation set or Haruspex gate sequence
//!   differs between arms, whose candidate reply was schema-invalid, or that base passes and the
//!   candidate fails, fails the PR.
//! - Stage 2: at most 4 fixtures in a fixed random order, one at a time, both arms; it stops at
//!   the first fixture base passes and the candidate fails twice.
//! - Pass: both stages clean, candidate cost at most 1.15 x base, zero guard incidents.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const STAGE2_MAX: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arm {
    Base,
    Candidate,
}

impl Arm {
    pub fn name(self) -> &'static str {
        match self {
            Arm::Base => "base",
            Arm::Candidate => "candidate",
        }
    }
}

/// What one arm of one fixture did, read from its run journal and the cost ledger.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ArmResult {
    pub status: String,
    pub escalations: Vec<String>,
    pub gates: Vec<String>,
    pub schema_ok: bool,
    /// The fixture's pass signal: the expected status, and its hidden tests passing.
    pub passed: bool,
    pub cost_micros: u64,
    pub incidents: u32,
    /// Spend by provider, for the self-ledger.
    pub by_provider: BTreeMap<String, u64>,
}

pub trait Arms {
    fn run(&self, fixture: &Fixture, arm: Arm) -> Result<ArmResult, String>;
}

/// One fixture in the store: `fixtures/<name>/fixture.toml` beside `task.md`, `repo.bundle` and
/// `hidden/` (design §12.4).
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub dir: PathBuf,
    /// The owner's repo the task came from: its approved checks and `[repo.sandbox]` apply.
    pub repo: String,
    pub base_commit: String,
    pub expected_status: String,
    pub image: String,
    /// Check name → the in-container command.
    pub checks: BTreeMap<String, String>,
}

pub fn fixtures(store: &Path) -> Result<Vec<Fixture>, String> {
    let root = store.join("fixtures");
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(&root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    for e in entries {
        let dir = e.map_err(|e| e.to_string())?.path();
        let f = dir.join("fixture.toml");
        if !f.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&f).map_err(|e| e.to_string())?;
        let mut fx: Fixture = toml::from_str(&text).map_err(|e| format!("{}: {e}", f.display()))?;
        fx.name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        fx.dir = dir;
        crate::config::repo_slug(&fx.repo).map_err(|e| format!("fixture {}: {e}", fx.name))?;
        if !crate::selfimprove::install::sha_ok(&fx.base_commit) {
            return Err(format!(
                "fixture {}: base_commit is not a commit id",
                fx.name
            ));
        }
        crate::sandbox::job::image_ref(&fx.image)
            .map_err(|e| format!("fixture {}: {e}", fx.name))?;
        out.push(fx);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The store must hold at least `min` fixtures, `min_nd` of them expecting `needs_decision`.
pub fn enough(f: &[Fixture], min: usize, min_nd: usize) -> Result<(), String> {
    let nd = f
        .iter()
        .filter(|x| x.expected_status == "needs_decision")
        .count();
    if f.len() < min || nd < min_nd {
        return Err(format!(
            "{} fixtures ({nd} expecting needs_decision); the eval needs at least {min} ({min_nd})",
            f.len()
        ));
    }
    Ok(())
}

/// A fixed shuffle of `n` indices from `seed` (xorshift; no clock, no randomness crate).
pub fn order(n: usize, seed: u64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        idx.swap(i, (x % (i as u64 + 1)) as usize);
    }
    idx
}

pub struct Outcome {
    pub passed: bool,
    pub reason: String,
    pub spend: Value,
}

fn record(spend: &mut Vec<Value>, stage: u8, fx: &Fixture, arm: Arm, r: &ArmResult) {
    spend.push(
        json!({ "stage": stage, "fixture": fx.name, "arm": arm.name(),
                       "cost_micros": r.cost_micros, "by_provider": r.by_provider }),
    );
}

/// Runs both stages over `fixtures` with `arms`, in an order fixed by `seed`.
pub fn evaluate(fixtures: &[Fixture], arms: &dyn Arms, seed: u64) -> Result<Outcome, String> {
    let mut spend = Vec::new();
    let (mut base_cost, mut cand_cost, mut incidents) = (0u64, 0u64, 0u32);
    let fail = |reason: String, spend: Vec<Value>| Outcome {
        passed: false,
        reason,
        spend: json!(spend),
    };
    for fx in fixtures {
        let b = arms.run(fx, Arm::Base)?;
        let c = arms.run(fx, Arm::Candidate)?;
        record(&mut spend, 1, fx, Arm::Base, &b);
        record(&mut spend, 1, fx, Arm::Candidate, &c);
        base_cost += b.cost_micros;
        cand_cost += c.cost_micros;
        incidents += c.incidents;
        let why = if c.escalations != b.escalations {
            Some(format!(
                "escalations {:?} became {:?}",
                b.escalations, c.escalations
            ))
        } else if c.gates != b.gates {
            Some(format!(
                "the gate sequence {:?} became {:?}",
                b.gates, c.gates
            ))
        } else if !c.schema_ok {
            Some("a schema-invalid reply".to_string())
        } else if b.passed && !c.passed {
            Some("base passes, the candidate fails".to_string())
        } else {
            None
        };
        if let Some(w) = why {
            return Ok(fail(format!("stage 1, fixture {}: {w}", fx.name), spend));
        }
    }
    for i in order(fixtures.len(), seed).into_iter().take(STAGE2_MAX) {
        let fx = &fixtures[i];
        let b = arms.run(fx, Arm::Base)?;
        record(&mut spend, 2, fx, Arm::Base, &b);
        base_cost += b.cost_micros;
        let mut cand_fails = 0;
        for attempt in 0..2 {
            let c = arms.run(fx, Arm::Candidate)?;
            record(&mut spend, 2, fx, Arm::Candidate, &c);
            // The cost rule compares paired runs: a second try is recorded, not counted.
            if attempt == 0 {
                cand_cost += c.cost_micros;
            }
            incidents += c.incidents;
            if c.passed {
                break;
            }
            cand_fails += 1;
        }
        if b.passed && cand_fails == 2 {
            return Ok(fail(
                format!(
                    "stage 2, fixture {}: base passes, the candidate fails twice",
                    fx.name
                ),
                spend,
            ));
        }
    }
    if incidents > 0 {
        return Ok(fail(
            format!("{incidents} guard incident(s) in the candidate arm"),
            spend,
        ));
    }
    // cost ≤ 1.15 × base, in integer micro-dollars: 100 × cand ≤ 115 × base.
    if u128::from(cand_cost) * 100 > u128::from(base_cost) * 115 {
        return Ok(fail(
            format!("the candidate cost {cand_cost} µ$ against base {base_cost} µ$ (over 1.15x)"),
            spend,
        ));
    }
    Ok(Outcome {
        passed: true,
        reason: format!("{} fixtures clean in stage 1 and stage 2", fixtures.len()),
        spend: json!(spend),
    })
}

/// What an arm's result line (`report::output`), run journal and ledger lines say.
pub fn arm_result(
    out: &Value,
    journal: &[Value],
    ledger: &[Value],
    run: &str,
    expected: &str,
    hidden_ok: bool,
) -> ArmResult {
    let status = out["status"].as_str().unwrap_or("unknown").to_string();
    let mut escalations: Vec<String> = out["escalations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["id"].as_str().map(str::to_string))
        .collect();
    escalations.sort();
    escalations.dedup();
    let gates = journal
        .iter()
        .filter(|e| e["kind"] == "gate_evaluated")
        .filter_map(|e| e["gate"].as_str().map(str::to_string))
        .collect();
    let schema_ok = !journal.iter().any(|e| {
        e["kind"] == "llm_retry" || (e["kind"] == "llm_finished" && e["outcome"] == "Invalid")
    });
    let mut by_provider = BTreeMap::new();
    let mut cost = 0;
    for l in ledger.iter().filter(|l| l["run"] == run) {
        let c = l["cost_micros"].as_u64().unwrap_or(0);
        cost += c;
        *by_provider
            .entry(l["provider"].as_str().unwrap_or("unknown").to_string())
            .or_insert(0) += c;
    }
    ArmResult {
        passed: status == expected && hidden_ok,
        status,
        escalations,
        gates,
        schema_ok,
        cost_micros: cost,
        incidents: journal.iter().filter(|e| e["kind"] == "incident").count() as u32,
        by_provider,
    }
}

#[cfg(test)]
#[path = "eval_live_tests.rs"]
mod tests;
