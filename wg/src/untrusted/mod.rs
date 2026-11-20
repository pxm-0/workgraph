//! Untrusted LLM output: `structured_output` is parsed into wire structs that deny unknown fields,
//! then checked by a semantic validator, then converted to the factory's `api` types (design §4.3).

pub mod reading;
pub mod review;
pub mod rules;
pub mod schemas;
pub mod small;

use serde::Deserialize;
use serde_json::Value;
use wg_factory::api::{Escalation, Reply, SchemaId, Severity};

/// What the validators need to know about the run so far.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub max_units: usize,
    /// The branch checked out when the run started; the Reader may keep it or create `work/<slug>`.
    pub start_branch: String,
    /// Criteria ids known from the Reading, the units and the challenge's additions.
    pub known_criteria: Vec<String>,
    /// Signatures of failures fixed in this run; the only ones the learner may phrase.
    pub fixed_sigs: Vec<String>,
}

/// Parses `structured_output` for `schema`. Every error names what failed, never the model's text.
pub fn parse(schema: SchemaId, v: &Value, ctx: &Ctx) -> Result<Reply, String> {
    if v.to_string().len() > 64 * 1024 {
        return Err("structured output is larger than 64 KB".into());
    }
    match schema {
        SchemaId::Reading => reading::parse(v, ctx).map(Reply::Reading),
        SchemaId::Challenge => small::challenge(v, ctx).map(Reply::Challenge),
        SchemaId::Work => small::work(v).map(Reply::Work),
        SchemaId::Review => review::parse(v, ctx).map(Reply::Review),
        SchemaId::Learn => small::learn(v, ctx).map(Reply::Learn),
    }
}

pub fn from_value<T: for<'de> Deserialize<'de>>(v: &Value, what: &str) -> Result<T, String> {
    T::deserialize(v).map_err(|e| format!("{what} does not match its schema: {e}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireEscalation {
    id: String,
    severity: String,
    question: String,
    recommendation: String,
    #[serde(default)]
    options: Vec<String>,
}

pub fn severity(s: &str) -> Result<Severity, String> {
    match s {
        "low" => Ok(Severity::Low),
        "medium" => Ok(Severity::Medium),
        "high" => Ok(Severity::High),
        "critical" => Ok(Severity::Critical),
        _ => Err(format!(
            "unknown severity {:?}",
            crate::ids::truncate(s, 20)
        )),
    }
}

/// Escalation text is capped (≤ 2 KB) and stripped of control characters; severity is high or
/// critical.
pub fn escalation(e: WireEscalation) -> Result<Escalation, String> {
    let sev = severity(&e.severity)?;
    if !sev.serious() {
        return Err(format!("escalation {} must be high or critical", e.id));
    }
    let id = crate::ids::one_line(&e.id, 32);
    if id.is_empty() {
        return Err("an escalation has no id".into());
    }
    Ok(Escalation {
        id,
        severity: sev,
        question: crate::ids::one_line(&e.question, 2048),
        recommendation: crate::ids::one_line(&e.recommendation, 2048),
        options: e
            .options
            .iter()
            .take(8)
            .map(|o| crate::ids::one_line(o, 200))
            .collect(),
    })
}

/// Every name in `items` must be unique.
pub fn unique<'a>(items: impl Iterator<Item = &'a str>, what: &str) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for i in items {
        if !seen.insert(i) {
            return Err(format!("{what} {i:?} appears twice"));
        }
    }
    Ok(())
}
