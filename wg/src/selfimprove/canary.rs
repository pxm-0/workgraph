//! The canary (design §12.5): the first shift on a newly installed binary is unhealthy if any of
//! these occur, read from that shift's journal and its runs' journals only: a panic or crash (the
//! shift never finished), any guard incident, schema-invalid replies above 30%, no task reaching a
//! PR when at least 2 were attempted, or any call that tripped the per-call runaway guard.

use serde_json::Value;

/// Why a shift on a new binary was unhealthy; empty when healthy.
pub fn problems(shift: &[Value], runs: &[Vec<Value>]) -> Vec<String> {
    let mut out = Vec::new();
    if !shift.iter().any(|e| e["kind"] == "shift_finished") {
        out.push("the shift never finished: a panic or crash".to_string());
    }
    let tasks: Vec<&Value> = shift
        .iter()
        .filter(|e| e["kind"] == "task_finished")
        .collect();
    let incidents = tasks.iter().filter(|t| t["outcome"] == "incident").count()
        + runs
            .iter()
            .flatten()
            .filter(|e| e["kind"] == "incident")
            .count();
    if incidents > 0 {
        out.push(format!("{incidents} guard incident(s)"));
    }
    let calls: Vec<&Value> = runs
        .iter()
        .flatten()
        .filter(|e| e["kind"] == "llm_finished")
        .collect();
    let invalid = calls.iter().filter(|e| e["outcome"] == "Invalid").count()
        + runs
            .iter()
            .flatten()
            .filter(|e| e["kind"] == "llm_retry")
            .count();
    if !calls.is_empty() && invalid * 10 > calls.len() * 3 {
        out.push(format!(
            "{invalid} of {} replies were schema-invalid (over 30%)",
            calls.len()
        ));
    }
    let prs = tasks.iter().filter(|t| t["outcome"] == "pr-open").count();
    if tasks.len() >= 2 && prs == 0 {
        out.push(format!("no task reached a PR in {} attempts", tasks.len()));
    }
    if calls.iter().any(|e| e["outcome"] == "BudgetHit") {
        out.push("a call tripped the per-call runaway guard".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shift(tasks: &[&str], finished: bool) -> Vec<Value> {
        let mut v: Vec<Value> = tasks
            .iter()
            .map(|o| json!({ "kind": "task_finished", "outcome": o }))
            .collect();
        if finished {
            v.push(json!({ "kind": "shift_finished" }));
        }
        v
    }

    fn calls(ok: usize, invalid: usize) -> Vec<Value> {
        (0..ok)
            .map(|_| json!({ "kind": "llm_finished", "outcome": "ok" }))
            .chain((0..invalid).map(|_| json!({ "kind": "llm_finished", "outcome": "Invalid" })))
            .collect()
    }

    #[test]
    fn each_sign_alone_makes_the_canary_unhealthy() {
        assert!(problems(&shift(&["pr-open", "stuck"], true), &[calls(10, 3)]).is_empty());
        assert!(
            problems(&shift(&[], true), &[]).is_empty(),
            "an idle shift is healthy"
        );
        for (s, r, want) in [
            (shift(&["pr-open"], false), vec![], "never finished"),
            (shift(&["incident"], true), vec![], "incident"),
            (
                shift(&["pr-open"], true),
                vec![vec![json!({ "kind": "incident" })]],
                "incident",
            ),
            (
                shift(&["pr-open"], true),
                vec![calls(6, 4)],
                "schema-invalid",
            ),
            (
                shift(&["pr-open"], true),
                vec![[calls(2, 0), vec![json!({ "kind": "llm_retry" }); 2]].concat()],
                "schema-invalid",
            ),
            (
                shift(&["stuck", "needs-decision"], true),
                vec![],
                "no task reached a PR",
            ),
            (
                shift(&["pr-open"], true),
                vec![vec![
                    json!({ "kind": "llm_finished", "outcome": "BudgetHit" }),
                ]],
                "runaway",
            ),
        ] {
            let p = problems(&s, &r);
            assert!(p.iter().any(|x| x.contains(want)), "{want}: {p:?}");
        }
    }
}
