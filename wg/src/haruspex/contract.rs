//! The task-file contract snapshot and its monotonicity check (design §9.4). After Read the kernel
//! records criterion ids, `required_checks` and every high or critical item; after Challenge the
//! file may only have grown; after that, any weakening is an incident unless an owner decision
//! names that item.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Contract {
    pub criteria: BTreeSet<String>,
    pub required_checks: BTreeSet<String>,
    /// High or critical failure modes, assumptions and questions: id → status.
    pub serious: BTreeMap<String, String>,
}

const ITEM_LISTS: [&str; 3] = ["failure_modes", "assumptions", "open_questions"];

fn text(v: &Value) -> String {
    v.as_str().unwrap_or("").to_lowercase()
}

fn is_serious(item: &Value) -> bool {
    let level = match item.get("severity") {
        Some(s) => text(s),
        None => text(&item["impact"]),
    };
    level == "high" || level == "critical"
}

pub fn snapshot(task: &Value) -> Contract {
    let criteria = task["acceptance_criteria"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| text(&c["status"]) != "waived")
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect();
    let required_checks = task["required_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_string))
        .collect();
    let serious = ITEM_LISTS
        .iter()
        .flat_map(|k| task[*k].as_array().into_iter().flatten())
        .filter(|i| is_serious(i))
        .filter_map(|i| Some((i["id"].as_str()?.to_string(), text(&i["status"]))))
        .collect();
    Contract {
        criteria,
        required_checks,
        serious,
    }
}

/// Serious items already `accepted_risk` that no owner decision answers: escalations after Read.
pub fn unanswered_acceptances(c: &Contract, decided: &[String]) -> Vec<String> {
    c.serious
        .iter()
        .filter(|(id, status)| *status == "accepted_risk" && !decided.contains(id))
        .map(|(id, _)| id.clone())
        .collect()
}

/// `Some(reason)` when `now` weakened `before`. `decided` lists item ids the owner answered.
pub fn weakened(before: &Contract, now: &Contract, decided: &[String]) -> Option<String> {
    let ok = |id: &String| decided.contains(id);
    if let Some(c) = before.criteria.difference(&now.criteria).find(|c| !ok(c)) {
        return Some(format!("criterion {c} was removed or waived"));
    }
    if let Some(c) = before
        .required_checks
        .difference(&now.required_checks)
        .find(|c| !ok(c))
    {
        return Some(format!("required check {c} was removed"));
    }
    for (id, status) in &before.serious {
        match now.serious.get(id) {
            None if !ok(id) => return Some(format!("{id} was removed or downgraded")),
            Some(s) if s == "accepted_risk" && status != "accepted_risk" && !ok(id) => {
                return Some(format!("{id} was moved to accepted_risk"))
            }
            _ => {}
        }
    }
    None
}

/// After Challenge the snapshot is the union of both.
pub fn union(a: &Contract, b: &Contract) -> Contract {
    let mut serious = a.serious.clone();
    serious.extend(b.serious.clone());
    Contract {
        criteria: a.criteria.union(&b.criteria).cloned().collect(),
        required_checks: a
            .required_checks
            .union(&b.required_checks)
            .cloned()
            .collect(),
        serious,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task() -> Value {
        json!({
            "acceptance_criteria": [{ "id": "AC-001", "status": "unverified" }, { "id": "AC-002", "status": "waived" }],
            "required_checks": ["test", "lint"],
            "failure_modes": [{ "id": "FM-001", "severity": "critical", "status": "open" },
                              { "id": "FM-002", "severity": "low", "status": "open" }],
            "assumptions": [{ "id": "ASM-001", "impact": "high", "status": "accepted_risk" }],
        })
    }

    #[test]
    fn snapshot_keeps_live_criteria_checks_and_serious_items() {
        let c = snapshot(&task());
        assert_eq!(c.criteria.iter().collect::<Vec<_>>(), ["AC-001"]);
        assert_eq!(c.required_checks.len(), 2);
        assert_eq!(c.serious.keys().collect::<Vec<_>>(), ["ASM-001", "FM-001"]);
        assert_eq!(unanswered_acceptances(&c, &[]), ["ASM-001"]);
        assert!(unanswered_acceptances(&c, &["ASM-001".into()]).is_empty());
    }

    #[test]
    fn each_weakening_is_caught_unless_the_owner_decided_it() {
        let before = snapshot(&task());
        let edit = |f: &dyn Fn(&mut Value)| {
            let mut t = task();
            f(&mut t);
            snapshot(&t)
        };
        type Edit<'a> = &'a dyn Fn(&mut Value);
        let cases: [(Edit, &str); 4] = [
            (
                &|t| t["acceptance_criteria"][0]["status"] = json!("waived"),
                "AC-001",
            ),
            (&|t| t["required_checks"] = json!(["test"]), "lint"),
            (
                &|t| t["failure_modes"][0]["status"] = json!("accepted_risk"),
                "FM-001",
            ),
            (
                &|t| t["failure_modes"][0]["severity"] = json!("low"),
                "FM-001",
            ),
        ];
        for (f, id) in cases {
            let now = edit(f);
            assert!(weakened(&before, &now, &[]).unwrap().contains(id), "{id}");
            assert_eq!(
                weakened(&before, &now, &[id.to_string()]),
                None,
                "{id} decided"
            );
        }
        let grown = edit(&|t| t["required_checks"] = json!(["test", "lint", "e2e"]));
        assert_eq!(weakened(&before, &grown, &[]), None);
        assert!(union(&before, &grown).required_checks.contains("e2e"));
    }
}
