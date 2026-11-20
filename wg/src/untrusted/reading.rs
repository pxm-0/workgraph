//! The Reading: the plan every later decision rests on, so it gets the strictest validator.

use super::{escalation, from_value, severity, unique, Ctx, WireEscalation};
use crate::ids;
use crate::paths::repo_path;
use serde::Deserialize;
use serde_json::Value;
use wg_factory::api::{CheckSpec, Criterion, Reading, Size, UnitPlan};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Wire {
    branch: String,
    #[serde(default)]
    note: Option<String>,
    task_id: String,
    initialized: bool,
    risk: String,
    checks: Vec<WireCheck>,
    criteria: Vec<WireCriterion>,
    units: Vec<WireUnit>,
    escalations: Vec<WireEscalation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCheck {
    name: String,
    command: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCriterion {
    id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WireUnit {
    id: String,
    goal: String,
    files: Vec<String>,
    depends_on: Vec<String>,
    criteria: Vec<String>,
    size: String,
    risky: bool,
    #[serde(default)]
    risk_reason: Option<String>,
}

pub fn parse(v: &Value, ctx: &Ctx) -> Result<Reading, String> {
    let w: Wire = from_value(v, "the Reading")?;
    ids::task_id(&w.task_id)?;
    if !ids::work_branch(&w.branch) && w.branch != ctx.start_branch {
        return Err(format!(
            "branch {:?} must be work/<slug> or the branch the run started on",
            ids::truncate(&w.branch, 60)
        ));
    }
    if w.units.len() > ctx.max_units {
        return Err(format!(
            "the Reading has {} units; at most {} are allowed",
            w.units.len(),
            ctx.max_units
        ));
    }
    unique(w.units.iter().map(|u| u.id.as_str()), "unit id")?;
    unique(w.checks.iter().map(|c| c.name.as_str()), "check name")?;
    unique(w.criteria.iter().map(|c| c.id.as_str()), "criterion id")?;
    let checks = w
        .checks
        .into_iter()
        .map(|c| {
            ids::check_name(&c.name)?;
            let command = c.command.trim().to_string();
            if command.is_empty() || command.len() > 512 || command.contains('\n') {
                return Err(format!("check {} needs a one-line command", c.name));
            }
            Ok(CheckSpec {
                name: c.name,
                command,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let unit_ids: Vec<String> = w.units.iter().map(|u| u.id.clone()).collect();
    let units = w
        .units
        .into_iter()
        .map(|u| unit(u, &unit_ids))
        .collect::<Result<Vec<_>, String>>()?;
    criteria_owned_once(&units, &w.criteria)?;
    shared_files_are_ordered(&units)?;
    Ok(Reading {
        branch: w.branch,
        note: w.note.map(|n| ids::one_line(&n, 500)),
        task_id: w.task_id,
        initialized: w.initialized,
        risk: severity(&w.risk)?,
        checks,
        criteria: w
            .criteria
            .into_iter()
            .map(|c| Criterion {
                id: ids::one_line(&c.id, 32),
                text: ids::one_line(&c.text, 500),
            })
            .collect(),
        units,
        escalations: w
            .escalations
            .into_iter()
            .map(escalation)
            .collect::<Result<_, _>>()?,
    })
}

fn unit(u: WireUnit, known: &[String]) -> Result<UnitPlan, String> {
    ids::unit_id(&u.id)?;
    if u.files.is_empty() {
        return Err(format!("unit {} lists no files", u.id));
    }
    let files = u
        .files
        .iter()
        .map(|f| {
            let p = repo_path(f)?;
            if p == ".haruspex" || p.starts_with(".haruspex/") {
                return Err(format!("unit {} may not plan edits under .haruspex/", u.id));
            }
            Ok(p)
        })
        .collect::<Result<Vec<_>, String>>()?;
    if let Some(d) = u.depends_on.iter().find(|d| !known.contains(d)) {
        return Err(format!(
            "unit {} depends on unknown unit {:?}",
            u.id,
            ids::truncate(d, 40)
        ));
    }
    let size = match u.size.as_str() {
        "small" => Size::Small,
        "large" => Size::Large,
        other => return Err(format!("unit {} has unknown size {other:?}", u.id)),
    };
    Ok(UnitPlan {
        goal: ids::one_line(&u.goal, 500),
        files,
        depends_on: u.depends_on,
        criteria: u.criteria.iter().map(|c| ids::one_line(c, 32)).collect(),
        size,
        risky: u.risky,
        risk_reason: u.risk_reason.map(|r| ids::one_line(&r, 200)),
        id: u.id,
    })
}

/// Every listed criterion belongs to exactly one unit, and no criterion is in two units.
fn criteria_owned_once(units: &[UnitPlan], criteria: &[WireCriterion]) -> Result<(), String> {
    let all: Vec<&str> = units
        .iter()
        .flat_map(|u| u.criteria.iter().map(String::as_str))
        .collect();
    unique(all.iter().copied(), "criterion assigned to a unit")?;
    match criteria.iter().find(|c| !all.contains(&c.id.as_str())) {
        Some(c) => Err(format!("criterion {} belongs to no unit", c.id)),
        None => Ok(()),
    }
}

/// Units that touch the same file must be one unit or depend on each other (transitively).
fn shared_files_are_ordered(units: &[UnitPlan]) -> Result<(), String> {
    let reaches = |from: &str, to: &str| {
        let mut stack = vec![from.to_string()];
        let mut seen = Vec::new();
        while let Some(id) = stack.pop() {
            if id == to {
                return true;
            }
            if seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            if let Some(u) = units.iter().find(|u| u.id == id) {
                stack.extend(u.depends_on.iter().cloned());
            }
        }
        false
    };
    for (i, a) in units.iter().enumerate() {
        for b in &units[i + 1..] {
            let shared = a.files.iter().find(|f| b.files.contains(f));
            if let Some(f) = shared {
                if !reaches(&a.id, &b.id) && !reaches(&b.id, &a.id) {
                    return Err(format!(
                        "units {} and {} both edit {f} but neither depends on the other",
                        a.id, b.id
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Ctx {
        Ctx {
            max_units: 5,
            start_branch: "main".into(),
            ..Ctx::default()
        }
    }

    fn reading(units: Value) -> Value {
        json!({ "branch": "work/x", "taskId": "T-001", "initialized": true, "risk": "medium",
                "checks": [{ "name": "test", "command": "npm test" }],
                "criteria": [{ "id": "AC-a", "text": "a works" }], "units": units, "escalations": [] })
    }

    fn unit(id: &str, files: Value, deps: Value, crit: Value) -> Value {
        json!({ "id": id, "goal": "g", "files": files, "dependsOn": deps, "criteria": crit,
                "size": "small", "risky": false })
    }

    #[test]
    fn a_valid_reading_parses() {
        let r = parse(
            &reading(json!([unit(
                "a",
                json!(["./src/a.js"]),
                json!([]),
                json!(["AC-a"])
            )])),
            &ctx(),
        )
        .unwrap();
        assert_eq!(r.units[0].files, ["src/a.js"]);
        assert_eq!(r.checks[0].name, "test");
    }

    #[test]
    fn each_rule_rejects_one_bad_reading() {
        let ok_unit = unit("a", json!(["src/a.js"]), json!([]), json!(["AC-a"]));
        let mut extra = reading(json!([ok_unit.clone()]));
        extra["surprise"] = json!(1);
        let mut bad_branch = reading(json!([ok_unit.clone()]));
        bad_branch["branch"] = json!("main2");
        let mut bad_check = reading(json!([ok_unit.clone()]));
        bad_check["checks"] = json!([{ "name": "Bad Name", "command": "x" }]);
        let mut low_esc = reading(json!([ok_unit.clone()]));
        low_esc["escalations"] =
            json!([{ "id": "Q-1", "severity": "low", "question": "q", "recommendation": "r" }]);
        let six: Vec<Value> = (0..6)
            .map(|i| {
                unit(
                    &format!("u{i}"),
                    json!([format!("f{i}")]),
                    json!([]),
                    json!([]),
                )
            })
            .collect();
        let cases = [
            (extra, "does not match its schema"),
            (bad_branch, "must be work/<slug>"),
            (bad_check, "check name"),
            (low_esc, "must be high or critical"),
            (reading(json!(six)), "at most 5"),
            (
                reading(json!([ok_unit.clone(), ok_unit.clone()])),
                "appears twice",
            ),
            (
                reading(json!([unit(
                    "a",
                    json!([".haruspex/x.json"]),
                    json!([]),
                    json!(["AC-a"])
                )])),
                "under .haruspex/",
            ),
            (
                reading(json!([unit(
                    "a",
                    json!(["../x"]),
                    json!([]),
                    json!(["AC-a"])
                )])),
                "must not contain ..",
            ),
            (
                reading(json!([unit(
                    "a",
                    json!(["src/a.js"]),
                    json!(["zz"]),
                    json!(["AC-a"])
                )])),
                "unknown unit",
            ),
            (
                reading(json!([unit(
                    "a",
                    json!(["src/a.js"]),
                    json!([]),
                    json!([])
                )])),
                "belongs to no unit",
            ),
            (
                reading(json!([
                    ok_unit,
                    unit("b", json!(["src/a.js"]), json!([]), json!([]))
                ])),
                "neither depends on the other",
            ),
        ];
        for (v, want) in cases {
            let err = parse(&v, &ctx()).unwrap_err();
            assert!(err.contains(want), "{err} should contain {want}");
        }
    }

    #[test]
    fn shared_files_are_fine_through_a_dependency_chain() {
        let units = json!([
            unit("a", json!(["src/x.js"]), json!([]), json!(["AC-a"])),
            unit("b", json!(["src/y.js"]), json!(["a"]), json!([])),
            unit("c", json!(["src/x.js"]), json!(["b"]), json!([])),
        ]);
        assert!(parse(&reading(units), &ctx()).is_ok());
    }
}
