//! The shift report's self-improvement part (design §7.5 item 4, §12.3): merged self PRs with
//! their revert command, held and refused PRs with the condition, shadow decisions awaiting the
//! owner's verdict, proposals filed, rollbacks and self-updates, and the eval's spend by provider
//! from the self-ledger. Built from the shift journal and the self-ledger only.

use crate::selfimprove::ledger::Ledger;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};

pub fn section(dirs: &Dirs, events: &[Value]) -> Result<Value, String> {
    let of = |kind: &str| -> Vec<Value> {
        events
            .iter()
            .filter(|e| e["kind"] == kind)
            .cloned()
            .collect()
    };
    let gates = of("self_gate");
    let with = |outcomes: &[&str]| -> Vec<Value> {
        gates
            .iter()
            .filter(|g| outcomes.iter().any(|o| g["outcome"] == *o))
            .cloned()
            .collect()
    };
    let start = events.first().and_then(|e| e["ts"].as_u64()).unwrap_or(0);
    let ledger = Ledger::at(&dirs.self_ledger());
    let merges = ledger.merges()?;
    let merged: Vec<Value> = with(&["merged"])
        .iter()
        .map(|g| {
            let m = merges.iter().rev().find(|m| m["pr"] == g["pr"]);
            let sha = m.and_then(|m| m["merge"].as_str()).unwrap_or("");
            json!({ "pr": g["pr"], "merge": sha, "revert": format!("git revert {sha}") })
        })
        .collect();
    let eval: Vec<Value> = ledger
        .of_kind("eval")?
        .into_iter()
        .filter(|e| e["at"].as_u64().unwrap_or(0) >= start)
        .map(|e| json!({ "pr": e["pr"], "passed": e["passed"], "spend": e["spend"] }))
        .collect();
    Ok(json!({
        "merged": merged,
        "held": with(&["held", "refused", "error"]),
        "shadow": with(&["shadow"]),
        "pending": with(&["pending"]),
        "lane": of("self_lane"),
        "proposals": of("proposal_filed"),
        "rolledBack": of("rolled_back"),
        "selfUpdate": of("self_update"),
        "eval": eval,
    }))
}

/// The owner's part of it, for "Needs you".
pub fn markdown(v: &Value) -> String {
    let lines = |k: &str, f: &dyn Fn(&Value) -> String| -> Vec<String> {
        v[k].as_array().into_iter().flatten().map(f).collect()
    };
    let mut out = Vec::new();
    out.extend(lines("rolledBack", &|r| {
        format!(
            "- rolled back: {}; the lane is paused until `wg self resume`",
            crate::ids::one_line(r["rollback"]["why"].as_str().unwrap_or(""), 300)
        )
    }));
    out.extend(lines("merged", &|m| {
        format!(
            "- merged self PR #{} as {}; to undo: `{}`",
            m["pr"],
            m["merge"].as_str().unwrap_or(""),
            m["revert"].as_str().unwrap_or("")
        )
    }));
    out.extend(lines("held", &|g| {
        format!(
            "- self PR #{} {}: {}",
            g["pr"],
            g["outcome"].as_str().unwrap_or(""),
            crate::ids::one_line(g["reason"].as_str().unwrap_or(""), 300)
        )
    }));
    out.extend(lines("shadow", &|g| {
        format!(
            "- self PR #{} would merge (shadow mode): merge or close it, and note your verdict",
            g["pr"]
        )
    }));
    out.extend(lines("proposals", &|p| {
        format!(
            "- factory proposal filed as {}#{} (wg:self-proposed, never worked)",
            p["repo"].as_str().unwrap_or(""),
            p["issue"]
        )
    }));
    out.extend(lines("eval", &|e| {
        format!(
            "- eval of #{}: passed {}, spend {}",
            e["pr"], e["passed"], e["spend"]
        )
    }));
    if out.is_empty() {
        "- none".into()
    } else {
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_held_shadow_and_proposals_reach_the_owner() {
        let home = std::env::temp_dir().join(format!("wg-report-self-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dirs = Dirs { home };
        Ledger::at(&dirs.self_ledger())
            .append("merged", json!({ "pr": 7, "merge": "abc" }))
            .unwrap();
        Ledger::at(&dirs.self_ledger())
            .append(
                "eval",
                json!({ "pr": 8, "passed": false, "spend": [{ "arm": "base" }] }),
            )
            .unwrap();
        let events = vec![
            json!({ "kind": "shift_started", "ts": 1 }),
            json!({ "kind": "self_gate", "pr": 7, "outcome": "merged", "reason": "x" }),
            json!({ "kind": "self_gate", "pr": 8, "outcome": "refused", "reason": "eval: stage 1" }),
            json!({ "kind": "self_gate", "pr": 9, "outcome": "shadow", "reason": "would merge" }),
            json!({ "kind": "proposal_filed", "repo": "pxm-0/workgraph", "issue": 501 }),
        ];
        let v = section(&dirs, &events).unwrap();
        assert_eq!(v["merged"][0]["revert"], "git revert abc");
        let md = markdown(&v);
        for want in [
            "#7 as abc",
            "`git revert abc`",
            "#8 refused: eval: stage 1",
            "#9 would merge",
            "pxm-0/workgraph#501",
            "eval of #8: passed false",
        ] {
            assert!(md.contains(want), "{want}: {md}");
        }
        assert_eq!(markdown(&json!({})), "- none");
    }
}
