//! The `--json-schema` sent with each call: run.js's schemas, with Reading `checks` as
//! `[{name, command}]`. A test generates a full example from each schema and checks that the wire
//! structs accept every field, so the schema and the parser cannot drift apart.

use serde_json::{json, Value};
use wg_factory::api::SchemaId;

fn s() -> Value {
    json!({ "type": "string" })
}

fn strs() -> Value {
    json!({ "type": "array", "items": s() })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
}

fn severity() -> Value {
    json!({ "type": "string", "enum": ["low", "medium", "high", "critical"] })
}

fn escalation() -> Value {
    obj(
        json!({ "id": s(), "severity": { "type": "string", "enum": ["high", "critical"] },
                "question": s(), "recommendation": s(), "options": strs() }),
        &["id", "severity", "question", "recommendation"],
    )
}

fn list(item: Value) -> Value {
    json!({ "type": "array", "items": item })
}

pub fn schema(id: SchemaId) -> Value {
    let b = json!({ "type": "boolean" });
    match id {
        SchemaId::Reading => obj(
            json!({
                "branch": s(), "note": s(), "taskId": s(), "initialized": b, "risk": severity(),
                "checks": list(obj(json!({ "name": s(), "command": s() }), &["name", "command"])),
                "criteria": list(obj(json!({ "id": s(), "text": s() }), &["id", "text"])),
                "units": list(obj(
                    json!({ "id": s(), "goal": s(), "files": strs(), "dependsOn": strs(),
                            "criteria": strs(), "size": { "type": "string", "enum": ["small", "large"] },
                            "risky": b, "riskReason": s() }),
                    &["id", "goal", "files", "dependsOn", "criteria", "size", "risky"],
                )),
                "escalations": list(escalation()),
            }),
            &[
                "branch",
                "taskId",
                "initialized",
                "risk",
                "checks",
                "criteria",
                "units",
                "escalations",
            ],
        ),
        SchemaId::Challenge => obj(
            json!({
                "added": list(obj(json!({ "id": s(), "kind": s(), "summary": s() }), &["id", "summary"])),
                "assign": list(obj(json!({ "criterionId": s(), "unitId": s() }), &["criterionId", "unitId"])),
                "escalations": list(escalation()),
            }),
            &["added", "assign", "escalations"],
        ),
        SchemaId::Work => obj(json!({ "summary": s(), "blocked": s() }), &["summary"]),
        SchemaId::Review => obj(
            json!({
                "passed": b,
                "criteria": list(obj(
                    json!({ "id": s(), "status": { "type": "string", "enum": ["passed", "failed"] }, "evidence": strs() }),
                    &["id", "status"],
                )),
                "findings": list(obj(
                    json!({ "id": s(), "severity": severity(), "file": s(), "summary": s() }),
                    &["id", "severity", "summary"],
                )),
            }),
            &["passed", "criteria", "findings"],
        ),
        SchemaId::Learn => obj(
            json!({
                "rules": list(obj(
                    json!({ "sig": s(), "text": s(), "scope": { "type": "string", "enum": ["planner", "worker", "all"] } }),
                    &["sig", "text", "scope"],
                )),
                "proposals": list(obj(
                    json!({ "category": { "type": "string", "enum": ["automated-check", "coding-standard", "navigation", "haruspex-profile", "factory"] },
                            "proposal": s(), "evidence": s(), "sig": s() }),
                    &["category", "proposal", "evidence"],
                )),
            }),
            &["rules", "proposals"],
        ),
        SchemaId::Verdict => obj(
            json!({
                "passed": b,
                "findings": list(obj(
                    json!({ "id": s(), "severity": severity(), "file": s(), "summary": s() }),
                    &["id", "severity", "summary"],
                )),
            }),
            &["passed", "findings"],
        ),
        SchemaId::ReleaseNotes => obj(
            json!({ "summary": s(), "changes": strs(), "breaking": b }),
            &["summary", "changes", "breaking"],
        ),
        SchemaId::ReleasePlan => obj(json!({ "ready": b, "summary": s() }), &["ready", "summary"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::untrusted::{parse, Ctx};

    /// An instance with every property present, enums at their first value.
    fn example(schema: &Value) -> Value {
        match schema["type"].as_str() {
            Some("object") => Value::Object(
                schema["properties"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), example(v)))
                    .collect(),
            ),
            Some("array") => json!([example(&schema["items"])]),
            Some("boolean") => json!(true),
            _ => schema["enum"].get(0).cloned().unwrap_or(json!("x")),
        }
    }

    #[test]
    fn every_schema_property_is_a_wire_field() {
        let ctx = Ctx {
            max_units: 5,
            start_branch: "x".into(),
            known_criteria: vec!["x".into()],
            fixed_sigs: vec!["x".into()],
        };
        for id in [
            SchemaId::Reading,
            SchemaId::Challenge,
            SchemaId::Work,
            SchemaId::Review,
            SchemaId::Learn,
            SchemaId::Verdict,
            SchemaId::ReleaseNotes,
            SchemaId::ReleasePlan,
        ] {
            let v = example(&schema(id));
            if let Err(e) = parse(id, &v, &ctx) {
                assert!(!e.contains("does not match its schema"), "{id:?}: {e}");
            }
        }
    }
}
