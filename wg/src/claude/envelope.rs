//! The `claude -p --output-format json` result envelope and its failure classes (design §5.3,
//! ADR-001 §1-6). Values come from `structured_output` only; the free-text `result` is never parsed.

// The Err deliberately carries the readable envelope fields for the ledger; one per call.
#![allow(clippy::result_large_err)]

use serde_json::Value;
use wg_factory::api::LlmFailure;

const MAX_ENVELOPE: usize = 4 * 1024 * 1024;

/// What the ledger and the tier check need from one envelope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Envelope {
    pub structured: Option<Value>,
    /// `total_cost_usd` in micro-dollars; `None` when absent ("unknown", never zero).
    pub cost_micros: Option<u64>,
    /// The `modelUsage` key with the most output tokens.
    pub model_used: Option<String>,
    pub turns: Option<u64>,
    pub session_id: Option<String>,
    pub api_error_status: Option<i64>,
}

/// A classified failure, with the envelope fields that were readable.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub class: LlmFailure,
    pub detail: String,
    pub envelope: Envelope,
}

/// Dollars to integer micro-dollars; no floats in cost math after this point.
pub fn micros(usd: f64) -> Option<u64> {
    (usd.is_finite() && usd >= 0.0).then(|| (usd * 1_000_000.0).round() as u64)
}

fn fields(v: &Value) -> Envelope {
    let model_used = v["modelUsage"].as_object().and_then(|m| {
        m.iter()
            .max_by_key(|(_, u)| u["outputTokens"].as_u64().unwrap_or(0))
            .map(|(k, _)| k.clone())
    });
    Envelope {
        structured: v
            .get("structured_output")
            .filter(|s| s.is_object())
            .cloned(),
        cost_micros: v["total_cost_usd"].as_f64().and_then(micros),
        model_used,
        turns: v["num_turns"].as_u64(),
        session_id: v["session_id"].as_str().map(str::to_string),
        api_error_status: v["api_error_status"].as_i64(),
    }
}

/// Classifies one finished `claude` process. `exit` is `None` when it was killed.
pub fn classify(exit: Option<i32>, timed_out: bool, stdout: &str) -> Result<Envelope, Failure> {
    let fail = |class, detail: &str, envelope| {
        Err(Failure {
            class,
            detail: detail.to_string(),
            envelope,
        })
    };
    if timed_out {
        return fail(
            LlmFailure::Timeout,
            "the call timed out",
            Envelope::default(),
        );
    }
    if stdout.len() > MAX_ENVELOPE {
        return fail(
            LlmFailure::Invalid,
            "the envelope is larger than 4 MB",
            Envelope::default(),
        );
    }
    let v: Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) if exit != Some(0) => {
            return fail(
                LlmFailure::Died,
                "claude exited without an envelope",
                Envelope::default(),
            )
        }
        Err(_) => {
            return fail(
                LlmFailure::Invalid,
                "stdout is not one JSON envelope",
                Envelope::default(),
            )
        }
    };
    let env = fields(&v);
    if v["type"] != "result" {
        return fail(LlmFailure::Invalid, "the envelope is not a result", env);
    }
    match v["subtype"].as_str() {
        Some("error_max_turns") => return fail(LlmFailure::TurnLimit, "turn limit reached", env),
        Some("error_max_budget_usd") => {
            return fail(
                LlmFailure::BudgetHit,
                "the per-call runaway guard tripped",
                env,
            )
        }
        _ => {}
    }
    // ADR-001 §2: API errors say subtype "success", so is_error is load-bearing.
    if v["is_error"] != Value::Bool(false) {
        // ADR-001 §5: the limited envelopes are unrecorded (#5); match the status, and treat any
        // other error (401 included) as Died, which parks safely.
        if env.api_error_status == Some(429) {
            return fail(LlmFailure::RateLimited, "rate or usage limited (429)", env);
        }
        return fail(LlmFailure::Died, "claude reported an error", env);
    }
    if exit != Some(0) {
        return fail(LlmFailure::Died, "claude exited non-zero", env);
    }
    if v["subtype"] != "success" {
        return fail(LlmFailure::Died, "the result is not a success", env);
    }
    if env.structured.is_none() {
        return fail(
            LlmFailure::Invalid,
            "no structured_output in the envelope",
            env,
        );
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn success_carries_structured_output_cost_and_the_dominant_model() {
        let out = env(
            json!({ "type": "result", "subtype": "success", "is_error": false,
            "structured_output": { "a": 1 }, "total_cost_usd": 0.0095, "num_turns": 2, "session_id": "s",
            "modelUsage": { "claude-haiku-5-5": { "outputTokens": 75 }, "claude-sonnet-5-5": { "outputTokens": 209 } } }),
        );
        let e = classify(Some(0), false, &out).unwrap();
        assert_eq!(e.structured, Some(json!({ "a": 1 })));
        assert_eq!(e.cost_micros, Some(9500));
        assert_eq!(e.model_used.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(e.turns, Some(2));
    }

    #[test]
    fn each_failure_class() {
        let base = |extra: Value| {
            let mut v = json!({ "type": "result", "subtype": "success", "is_error": false, "structured_output": {} });
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            env(v)
        };
        let cases = [
            (Some(0), true, base(json!({})), LlmFailure::Timeout),
            (Some(1), false, "boom".to_string(), LlmFailure::Died),
            (Some(0), false, "not json".to_string(), LlmFailure::Invalid),
            (
                Some(1),
                false,
                base(json!({ "subtype": "error_max_turns", "is_error": true })),
                LlmFailure::TurnLimit,
            ),
            (
                Some(1),
                false,
                base(json!({ "subtype": "error_max_budget_usd", "is_error": true })),
                LlmFailure::BudgetHit,
            ),
            (
                Some(1),
                false,
                base(json!({ "is_error": true, "api_error_status": 429 })),
                LlmFailure::RateLimited,
            ),
            (
                Some(1),
                false,
                base(json!({ "is_error": true, "api_error_status": 401 })),
                LlmFailure::Died,
            ),
            (
                Some(1),
                false,
                base(json!({ "is_error": true, "api_error_status": 404 })),
                LlmFailure::Died,
            ),
            (Some(2), false, base(json!({})), LlmFailure::Died),
            (
                Some(0),
                false,
                base(json!({ "structured_output": null })),
                LlmFailure::Invalid,
            ),
            (
                Some(0),
                false,
                base(json!({ "type": "assistant" })),
                LlmFailure::Invalid,
            ),
            (
                Some(0),
                false,
                "x".repeat(MAX_ENVELOPE + 1),
                LlmFailure::Invalid,
            ),
        ];
        for (exit, timed_out, out, want) in cases {
            let got = classify(exit, timed_out, &out).unwrap_err().class;
            assert_eq!(got, want, "{}", &out[..out.len().min(120)]);
        }
    }

    #[test]
    fn a_missing_cost_is_unknown_not_zero() {
        let out = env(
            json!({ "type": "result", "subtype": "success", "is_error": false, "structured_output": {} }),
        );
        assert_eq!(classify(Some(0), false, &out).unwrap().cost_micros, None);
        assert_eq!(micros(f64::NAN), None);
    }
}
