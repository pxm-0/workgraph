//! The per-repo memory file: the v1 validator, read, and an atomic save with a `.bak`.

use serde_json::{json, Value};
use std::fs;

pub const MEMORY: &str = ".claude/work-memory.json";

/// The memory file's text, or `None` when it does not exist.
pub fn read() -> Result<Option<String>, String> {
    match fs::read_to_string(MEMORY) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {MEMORY}: {e}")),
    }
}

fn is_rule(r: &Value) -> bool {
    let positive = |k: &str| {
        r.get(k)
            .and_then(Value::as_f64)
            .is_some_and(|x| x.is_finite() && x > 0.0)
    };
    ["id", "text", "sig"]
        .iter()
        .all(|k| r.get(*k).is_some_and(Value::is_string))
        && matches!(
            r.get("scope").and_then(Value::as_str),
            Some("planner" | "worker" | "all")
        )
        && positive("a")
        && positive("b")
}

pub fn validate(m: &Value) -> Result<(), String> {
    let o = m.as_object().ok_or("memory must be a JSON object")?;
    if o.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("memory.version must be 1".into());
    }
    for k in ["runs", "nextId"] {
        if !o.get(k).is_some_and(Value::is_u64) {
            return Err(format!("memory.{k} must be a non-negative integer"));
        }
    }
    if !o
        .get("checks")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().all(Value::is_string))
    {
        return Err("memory.checks must be an array of strings".into());
    }
    if !o
        .get("rules")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().all(is_rule))
    {
        return Err("memory.rules must be an array of valid rules".into());
    }
    if !o.get("models").is_some_and(Value::is_object) {
        return Err("memory.models must be an object".into());
    }
    Ok(())
}

/// Validates `memory`, backs up the current file and replaces it atomically.
pub fn save(memory: &Value) -> Result<Value, String> {
    validate(memory)?;
    if let Ok(existing) = fs::read_to_string(MEMORY) {
        // Never overwrite a memory file we can't parse: it may hold rules worth keeping.
        serde_json::from_str::<Value>(&existing)
            .map_err(|e| format!("{MEMORY} exists but is not valid JSON ({e}); left untouched"))?;
        fs::write(format!("{MEMORY}.bak"), &existing)
            .map_err(|e| format!("cannot write backup: {e}"))?;
    }
    fs::create_dir_all(".claude").map_err(|e| format!("cannot create .claude: {e}"))?;
    let tmp = format!("{MEMORY}.tmp.{}", std::process::id());
    fs::write(
        &tmp,
        serde_json::to_string_pretty(memory).expect("Value serializes") + "\n",
    )
    .map_err(|e| format!("cannot write {tmp}: {e}"))?;
    fs::rename(&tmp, MEMORY).map_err(|e| format!("cannot replace {MEMORY}: {e}"))?;
    Ok(
        json!({ "saved": true, "path": MEMORY, "rules": memory["rules"].as_array().map_or(0, Vec::len) }),
    )
}

#[cfg(test)]
mod tests {
    use super::validate;
    use serde_json::{json, Value};

    fn valid() -> Value {
        json!({ "version": 1, "runs": 0, "nextId": 1, "checks": ["npm test"],
                "rules": [{ "id": "r1", "text": "t", "scope": "all", "sig": "s", "a": 2, "b": 1 }],
                "models": {} })
    }

    fn with(key: &str, value: Value) -> Value {
        let mut m = valid();
        m[key] = value;
        m
    }

    #[test]
    fn each_rule_of_the_v1_schema_rejects_one_bad_value() {
        assert_eq!(validate(&valid()), Ok(()));
        let bad_rule =
            json!([{ "id": "r1", "text": "t", "scope": "all", "sig": "s", "a": 0, "b": 1 }]);
        let cases = [
            (json!([]), "memory must be a JSON object"),
            (with("version", json!(2)), "memory.version must be 1"),
            (
                with("runs", json!(-1)),
                "memory.runs must be a non-negative integer",
            ),
            (
                with("nextId", json!("1")),
                "memory.nextId must be a non-negative integer",
            ),
            (
                with("checks", json!([1])),
                "memory.checks must be an array of strings",
            ),
            (
                with("rules", bad_rule),
                "memory.rules must be an array of valid rules",
            ),
            (with("models", json!([])), "memory.models must be an object"),
        ];
        for (memory, expected) in cases {
            assert_eq!(validate(&memory), Err(expected.to_string()), "{memory}");
        }
    }
}
