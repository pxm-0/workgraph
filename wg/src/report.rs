//! The run's one JSON line and its `report.md`. The gate, `verified` and `reviewed` come from the
//! kernel's tokens, never from the factory's outcome (design §9.5, §11.1); spend comes from the
//! ledger, split by stage and provider, with unknown costs counted apart (design §5.3).

use crate::engine::convert::{escalations, outcome};
use crate::engine::driver::{End, Ran};
use crate::engine::RunCtx;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

fn usd(micros: u64) -> f64 {
    micros as f64 / 1_000_000.0
}

/// Spend of this run per `stage/provider`, from the cost ledger.
pub fn spend(ctx: &RunCtx) -> Result<(Value, u64, u64), String> {
    let (known, unknown, lines) = crate::budget::run_spend(&ctx.dirs.ledger(), &ctx.run_id)?;
    let mut by: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    for l in &lines {
        let key = format!(
            "{}/{}",
            l["stage"].as_str().unwrap_or("?"),
            l["provider"].as_str().unwrap_or("?")
        );
        let e = by.entry(key).or_default();
        e.0 += 1;
        match l["cost_micros"].as_u64() {
            Some(c) => e.1 += c,
            None => e.2 += 1,
        }
    }
    let rows: Map<String, Value> = by
        .into_iter()
        .map(|(k, (calls, micros, unknown))| {
            (
                k,
                json!({ "calls": calls, "usd": usd(micros), "unknownCost": unknown }),
            )
        })
        .collect();
    Ok((Value::Object(rows), known, unknown))
}

pub fn output(ctx: &RunCtx, ran: &Ran) -> Result<Value, String> {
    let k = &ran.kernel;
    let (by_stage, known, unknown) = spend(ctx)?;
    let reading = k.reading.as_ref();
    let mut o = Map::new();
    let haruspex = json!({
        "taskId": k.task_id(),
        "initialized": reading.is_some_and(|r| r.initialized) || k.initializing,
        "risk": reading.map(|r| r.risk.name()),
        "challengeAdded": k.challenge_added.iter().map(|a| json!({ "id": a.id, "kind": a.kind, "summary": a.summary })).collect::<Vec<_>>(),
    });
    let next = match &ran.end {
        End::Finish(out) => {
            o = outcome(out);
            let tree = ctx.ports.repo.tree()?;
            o.insert("verified".into(), json!(k.verified_at(&tree)));
            o.insert(
                "reviewed".into(),
                json!(k.review_pass.as_ref().is_some_and(|r| r.tree() == tree)),
            );
            "Nothing is committed. On the user's approval, run wg finalize --run with this runId."
        }
        End::NeedsDecision(list) => {
            o.insert("status".into(), json!("needs_decision"));
            o.insert("escalations".into(), escalations(list));
            o.insert("userChangesBefore".into(), json!(k.user_changes));
            "Nothing was built. Ask the user each escalation, then rerun with --task-id, --initialized, --user-changes-file and --decisions-file."
        }
        End::Incident(why) => {
            o.insert("status".into(), json!("failed"));
            o.insert("incident".into(), json!(why));
            "An incident aborted the run. Edits so far are uncommitted in the working tree; read the journal before anything else."
        }
        End::Parked(why) => {
            o.insert("status".into(), json!("parked"));
            o.insert("note".into(), json!(why));
            "The run parked; edits so far are uncommitted in the working tree."
        }
        End::Failed(why) => {
            o.insert("status".into(), json!("failed"));
            o.insert("note".into(), json!(why));
            "The run failed; edits so far, if any, are uncommitted in the working tree."
        }
    };
    if !o.contains_key("verified") {
        o.insert("verified".into(), json!(false));
        o.insert("reviewed".into(), json!(false));
    }
    o.insert("branch".into(), json!(reading.map(|r| r.branch.clone())));
    o.insert("taskId".into(), json!(k.task_id()));
    o.insert("initialized".into(), haruspex["initialized"].clone());
    o.insert("risk".into(), haruspex["risk"].clone());
    o.insert("haruspex".into(), haruspex);
    o.insert("checks".into(), json!(k.gate));
    o.insert("checksAlreadyFailing".into(), json!(k.left_out));
    o.insert("memorySaved".into(), json!(ran.memory_saved));
    o.insert(
        "rulesRejected".into(),
        json!(ran
            .rejected_rules
            .iter()
            .map(|(id, why)| json!({ "id": id, "why": why }))
            .collect::<Vec<_>>()),
    );
    o.insert("runId".into(), json!(ctx.run_id));
    o.insert("costUsd".into(), json!(usd(known)));
    o.insert("callsWithUnknownCost".into(), json!(unknown));
    o.insert("spend".into(), by_stage);
    o.insert("logDir".into(), json!(ctx.run_dir));
    o.insert("next".into(), json!(next));
    Ok(Value::Object(o))
}

/// A short human report next to the journal.
pub fn markdown(out: &Value) -> String {
    let list = |v: &Value| {
        v.as_array()
            .map(|a| {
                a.iter()
                    .map(|x| format!("- {x}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    format!(
        "# Run {}\n\nStatus: {} | verified: {} | reviewed: {}\nTask: {} on {}\nCost: ${} (calls with unknown cost: {})\n\n## Spend by stage/provider\n\n```json\n{}\n```\n\n## Done\n{}\n\n## Stuck\n{}\n\n## Skipped\n{}\n",
        out["runId"].as_str().unwrap_or(""),
        out["status"].as_str().unwrap_or(""),
        out["verified"],
        out["reviewed"],
        out["taskId"].as_str().unwrap_or(""),
        out["branch"],
        out["costUsd"],
        out["callsWithUnknownCost"],
        serde_json::to_string_pretty(&out["spend"]).unwrap_or_default(),
        list(&out["done"]),
        list(&out["stuck"]),
        list(&out["skipped"]),
    )
}
