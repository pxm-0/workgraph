//! The approval invariant (design §9.4): outside `haruspex/finalize.rs`, no approval transitions to
//! `approved`, and no `by` or `at` changes to a non-null value. Resets to `pending`/null are fine:
//! `task create` and `task activate` do exactly that during a legitimate Read.

use serde_json::Value;

/// `Some(reason)` when `after` shows an approval no agent may make.
pub fn violation(before: &Value, after: &Value) -> Option<String> {
    let entries = after.as_object()?;
    for (gate, a) in entries {
        let b = &before[gate.as_str()];
        if a["status"] == "approved" && b["status"] != "approved" {
            return Some(format!("approval {gate} became approved outside finalize"));
        }
        for field in ["by", "at"] {
            let changed = !a[field].is_null() && a[field] != b[field];
            if changed {
                return Some(format!("approval {gate}.{field} was set outside finalize"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::violation;
    use serde_json::json;

    #[test]
    fn resets_pass_and_approvals_fail() {
        let approved = json!({ "ready_to_build": { "status": "approved", "by": "o", "at": "t1" } });
        let pending = json!({ "ready_to_build": { "status": "pending", "by": null, "at": null } });
        assert_eq!(
            violation(&approved, &pending),
            None,
            "task create resets approvals"
        );
        assert_eq!(violation(&pending, &pending), None);
        assert_eq!(
            violation(&approved, &approved),
            None,
            "an approval from before the run"
        );
        assert!(
            violation(&pending, &approved).is_some(),
            "a planted approval"
        );
        let by_only =
            json!({ "ready_to_build": { "status": "pending", "by": "agent", "at": null } });
        assert!(violation(&pending, &by_only).unwrap().contains(".by"));
        let new_gate = json!({ "ready_to_release": { "status": "approved" } });
        assert!(violation(&json!(null), &new_gate).is_some());
        assert_eq!(violation(&pending, &json!(null)), None);
    }
}
