//! The generic stage kinds after dev (design §6.1, Phase 4): `checks`, `agent`, `review`,
//! `sandbox`, `haruspex_gate`, `human` and `publish`. Each is validated here, closed set by closed
//! set: a new stage of an existing kind needs TOML and a prompt only; a new kind is a Rust change.

use super::{llm, Stage, PUBLISH_LABELS};
use crate::assets::Assets;
use wg_factory::api::{Chain, Gate, OnFail, PublishDef, Role, SchemaId, StageDef, StageKind, Tier};
use wg_factory::engine::vars::AGENT_VARS;

/// Label prefixes the dev stages use; a later stage's prefix may not be one of them.
const RESERVED: [&str; 10] = [
    "read",
    "challenge",
    "escalate",
    "baseline",
    "build",
    "review",
    "learn",
    "work",
    "fix",
    "check",
];

/// The Haruspex gates a stage may evaluate or wait at. `ready-to-build` is the kernel's own and
/// `verification` is recorded only by finalize, with the owner's approval (design §9.1).
pub const GATES: [&str; 1] = ["ready-to-release"];

/// Step sets a `sandbox` stage may run, by their `[repo.sandbox]` key.
pub const STEP_SETS: [&str; 1] = ["release_rehearsal"];

pub struct Ctx<'a> {
    pub assets: &'a Assets,
    pub repo: &'a super::RepoView,
    /// A dev pipeline comes first, so there are built units to blame and fix.
    pub built: bool,
}

/// `re-review` → `rereview`: the prefix of the stage's call labels.
fn label(id: &str) -> Result<String, String> {
    let ok_id = (1..=32).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.chars().all(|c| c.is_ascii_lowercase() || c == '-');
    let l: String = id.chars().filter(|c| *c != '-').collect();
    if !ok_id || l.len() > 24 {
        return Err(format!(
            "stage id {id:?} must be lowercase letters and hyphens, at most 24 letters"
        ));
    }
    if RESERVED.contains(&l.as_str()) {
        return Err(format!("stage id {id:?} would reuse the dev labels {l}"));
    }
    Ok(l)
}

fn on_fail(s: &Stage, fixable: bool) -> Result<OnFail, String> {
    match s.on_fail.as_deref() {
        None | Some("stop") => Ok(OnFail::Stop),
        Some("fix") if fixable => Ok(OnFail::Fix),
        Some("fix") => Err(format!(
            "stage {}: on_fail = \"fix\" needs built units to fix (a dev pipeline first) and something to blame",
            s.id
        )),
        Some(o) => Err(format!("stage {}: on_fail must be fix or stop, not {o:?}", s.id)),
    }
}

fn gate_name(s: &Stage) -> Result<String, String> {
    match s.gate.as_ref().and_then(|g| g.as_str()) {
        Some(g) if GATES.contains(&g) => Ok(g.to_string()),
        _ => Err(format!(
            "stage {}: gate must be one of {}",
            s.id,
            GATES.join(", ")
        )),
    }
}

fn one_line(s: &str, what: &str) -> Result<String, String> {
    if s.trim().is_empty() || s.len() > 300 || s.chars().any(char::is_control) {
        return Err(format!("{what} must be one line of at most 300 bytes"));
    }
    Ok(s.to_string())
}

/// `role` and `schema` of an agent stage, a pair from a closed set.
fn agent_role(s: &Stage) -> Result<(Role, SchemaId), String> {
    let role = s.role.as_deref().unwrap_or("");
    let schema = s.schema.as_deref().unwrap_or("");
    match (role, schema) {
        ("qa-explorer", "verdict") => Ok((Role::QaExplorer, SchemaId::Verdict)),
        ("release-writer", "release-notes") => Ok((Role::ReleaseWriter, SchemaId::ReleaseNotes)),
        ("release-writer", "release-plan") => Ok((Role::ReleaseWriter, SchemaId::ReleasePlan)),
        _ => Err(format!(
            "stage {}: role and schema must be qa-explorer + verdict, or release-writer + release-notes or release-plan",
            s.id
        )),
    }
}

/// The boolean reply fields an agent's gate may read.
fn gate_field(schema: SchemaId) -> &'static [&'static str] {
    match schema {
        SchemaId::Verdict => &["passed"],
        SchemaId::ReleaseNotes => &["breaking"],
        SchemaId::ReleasePlan => &["ready"],
        _ => &[],
    }
}

fn agent(s: &Stage, c: &Ctx) -> Result<StageKind, String> {
    let (role, schema) = agent_role(s)?;
    let llm = llm(c.assets, s, s.role.as_deref().unwrap_or(""), AGENT_VARS)?;
    let gate = match &s.gate {
        None => None,
        Some(v) => {
            let t = v
                .as_table()
                .ok_or(format!("stage {}: gate must be {{ field, equals }}", s.id))?;
            let field = t.get("field").and_then(|f| f.as_str()).unwrap_or("");
            let equals = t.get("equals").and_then(|e| e.as_bool());
            match equals {
                Some(equals) if t.len() == 2 && gate_field(schema).contains(&field) => Some(Gate {
                    field: field.to_string(),
                    equals,
                }),
                _ => {
                    return Err(format!(
                        "stage {}: gate must be {{ field, equals }} on one of {}",
                        s.id,
                        gate_field(schema).join(", ")
                    ))
                }
            }
        }
    };
    // Only a verdict names findings to blame, and only built units can be fixed.
    let fixable = c.built && schema == SchemaId::Verdict && gate.is_some();
    Ok(StageKind::Agent {
        role,
        llm,
        schema,
        gate,
        on_fail: on_fail(s, fixable)?,
    })
}

/// One stage after dev, validated.
pub fn stage(s: &Stage, c: &Ctx) -> Result<StageDef, String> {
    let label = label(&s.id)?;
    let kind = match s.kind.as_str() {
        "checks" => {
            if s.set.as_deref() != Some("qa") {
                return Err(format!("stage {}: set must be \"qa\"", s.id));
            }
            let on_fail = on_fail(s, c.built)?;
            let max_fix_rounds = s.max_fix_rounds.unwrap_or(1);
            if max_fix_rounds > 3 || (on_fail == OnFail::Fix && max_fix_rounds == 0) {
                return Err(format!("stage {}: max_fix_rounds must be 1-3", s.id));
            }
            StageKind::Checks {
                names: c.repo.qa_checks.clone(),
                on_fail,
                max_fix_rounds,
            }
        }
        "agent" => agent(s, c)?,
        "review" => {
            let llm = llm(c.assets, s, "review", crate::render::VARS)?;
            let max_fix_rounds = s.max_fix_rounds.unwrap_or(1);
            if llm.tier != Tier::Opus || max_fix_rounds > 1 || !c.built {
                return Err(format!(
                    "stage {}: a review uses opus, at most one fix round, after dev",
                    s.id
                ));
            }
            StageKind::Review {
                llm,
                max_fix_rounds,
            }
        }
        "sandbox" => {
            let from = s.steps_from.as_deref().unwrap_or("");
            if !STEP_SETS.contains(&from) {
                return Err(format!(
                    "stage {}: steps_from must be one of {}",
                    s.id,
                    STEP_SETS.join(", ")
                ));
            }
            if !c.repo.rehearsal {
                return Err(format!(
                    "stage {}: [repo.sandbox].{from} has no steps for this repo",
                    s.id
                ));
            }
            let always = match s.when.as_deref() {
                Some("always") => true,
                None | Some("risky") => false,
                Some(w) => {
                    return Err(format!(
                        "stage {}: when must be always or risky, not {w:?}",
                        s.id
                    ))
                }
            };
            StageKind::Sandbox {
                steps_from: from.to_string(),
                always,
            }
        }
        "haruspex_gate" => StageKind::HaruspexGate {
            gate: gate_name(s)?,
            expect_only: s
                .expect_only
                .clone()
                .unwrap_or_default()
                .iter()
                .map(|b| one_line(b, &format!("stage {}: expect_only", s.id)))
                .collect::<Result<_, _>>()?,
        },
        "human" => StageKind::Human {
            gate: gate_name(s)?,
            ask: match &s.ask {
                Some(a) => one_line(a, &format!("stage {}: ask", s.id))?,
                None => format!("The owner decides {}.", gate_name(s)?),
            },
        },
        "publish" => StageKind::Publish(publish(s)?),
        k if super::ORDER.contains(&k) => {
            return Err(format!("stage {}: {k} runs only in the dev pipeline", s.id))
        }
        k => return Err(format!("stage {}: unknown kind {k:?}", s.id)),
    };
    Ok(StageDef {
        id: s.id.clone(),
        label,
        kind,
    })
}

/// The publish stage: always a draft (only `wg approve` makes a shift PR ready), labels only from
/// `PUBLISH_LABELS`.
fn publish(s: &Stage) -> Result<PublishDef, String> {
    if s.draft == Some(false) {
        return Err(format!(
            "stage {}: a shift PR is always a draft; only wg approve makes it ready",
            s.id
        ));
    }
    let labels = s.labels.clone().unwrap_or_default();
    for l in &labels {
        crate::ids::gh_label(l).map_err(|e| format!("stage {}: {e}", s.id))?;
        if !PUBLISH_LABELS.contains(&l.as_str()) {
            return Err(format!(
                "stage {}: a shift PR may carry only {}, not {l}",
                s.id,
                PUBLISH_LABELS.join(", ")
            ));
        }
    }
    Ok(PublishDef {
        draft: true,
        labels,
    })
}

/// Invariant 3 (design §6.3): at most one publish; it comes after the build and after a review that
/// follows every fix-capable stage (build, or checks and agents with `on_fail = "fix"`).
pub fn publish_after_review(c: &Chain) -> Result<(), String> {
    // Dev ends with its own review after the build.
    let mut unreviewed = false;
    let mut published = false;
    for s in &c.stages {
        match &s.kind {
            StageKind::Checks { on_fail, .. } | StageKind::Agent { on_fail, .. } => {
                unreviewed |= *on_fail == OnFail::Fix
            }
            StageKind::Review { .. } => unreviewed = false,
            StageKind::Publish(_) if published => {
                return Err("a chain has at most one publish stage".into())
            }
            StageKind::Publish(_) if c.dev.is_none() => {
                return Err(format!(
                    "stage {}: publish needs the dev build before it",
                    s.id
                ))
            }
            StageKind::Publish(_) if unreviewed => {
                return Err(format!(
                    "stage {}: publish must follow a review after every stage that can fix code",
                    s.id
                ))
            }
            StageKind::Publish(_) => published = true,
            _ => {}
        }
    }
    Ok(())
}
