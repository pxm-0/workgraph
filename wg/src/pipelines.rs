//! Load and validate a repo's chain of pipelines (design §6.1, §6.3): `dev` first, then later
//! pipelines of generic stages (`stages.rs`), or the release line alone. The invariants are a
//! backstop: publish and finalize still need the kernel's tokens whatever the files say.

mod stages;

pub use stages::{GATES, STEP_SETS};

use crate::assets::Assets;
use crate::config::MAX_TURNS;
use serde::Deserialize;
use wg_factory::api::{Chain, Effort, LlmStage, PipelineDef, StageKind, Tier};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    pipeline: Header,
    stage: Vec<Stage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    name: String,
    #[serde(default = "yes")]
    haruspex: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Stage {
    id: String,
    kind: String,
    model: Option<String>,
    effort: Option<String>,
    prompt: Option<String>,
    max_turns: Option<u32>,
    timeout_secs: Option<u32>,
    optional: Option<bool>,
    max_units: Option<u32>,
    max_fix_rounds: Option<u32>,
    worker_prompt: Option<String>,
    fix_prompt: Option<String>,
    draft: Option<bool>,
    labels: Option<Vec<String>>,
    set: Option<String>,
    on_fail: Option<String>,
    role: Option<String>,
    schema: Option<String>,
    /// A `{ field, equals }` table for `agent`, a gate name for `haruspex_gate` and `human`.
    gate: Option<toml::Value>,
    steps_from: Option<String>,
    when: Option<String>,
    expect_only: Option<Vec<String>>,
    ask: Option<String>,
}

/// Per-role ceilings from design §5.2: (max turns, timeout seconds).
fn ceiling(kind: &str) -> (u32, u32) {
    match kind {
        "read" => (60, 1800),
        "challenge" => (30, 900),
        "build" => (80, 1800),
        "review" => (60, 1800),
        "qa-explorer" => (60, 1200),
        "release-writer" => (40, 900),
        _ => (3, 600),
    }
}

const ORDER: [&str; 7] = [
    "read",
    "challenge",
    "escalate",
    "baseline",
    "build",
    "review",
    "learn",
];

fn keys_set(s: &Stage) -> Vec<&'static str> {
    [
        (s.model.is_some(), "model"),
        (s.effort.is_some(), "effort"),
        (s.prompt.is_some(), "prompt"),
        (s.max_turns.is_some(), "max_turns"),
        (s.timeout_secs.is_some(), "timeout_secs"),
        (s.optional.is_some(), "optional"),
        (s.max_units.is_some(), "max_units"),
        (s.max_fix_rounds.is_some(), "max_fix_rounds"),
        (s.worker_prompt.is_some(), "worker_prompt"),
        (s.fix_prompt.is_some(), "fix_prompt"),
        (s.draft.is_some(), "draft"),
        (s.labels.is_some(), "labels"),
        (s.set.is_some(), "set"),
        (s.on_fail.is_some(), "on_fail"),
        (s.role.is_some(), "role"),
        (s.schema.is_some(), "schema"),
        (s.gate.is_some(), "gate"),
        (s.steps_from.is_some(), "steps_from"),
        (s.when.is_some(), "when"),
        (s.expect_only.is_some(), "expect_only"),
        (s.ask.is_some(), "ask"),
    ]
    .into_iter()
    .filter_map(|(on, k)| on.then_some(k))
    .collect()
}

const LLM_KEYS: [&str; 5] = ["model", "effort", "prompt", "max_turns", "timeout_secs"];

fn allowed_keys(kind: &str) -> Vec<&'static str> {
    let mut k: Vec<&'static str> = match kind {
        "read" | "challenge" | "review" | "learn" | "agent" => LLM_KEYS.to_vec(),
        _ => vec![],
    };
    k.extend_from_slice(match kind {
        "challenge" => &["optional"][..],
        "build" => &[
            "max_units",
            "max_fix_rounds",
            "max_turns",
            "timeout_secs",
            "worker_prompt",
            "fix_prompt",
        ],
        "review" => &["max_fix_rounds"],
        "learn" => &[],
        "publish" => &["draft", "labels"],
        "checks" => &["set", "on_fail", "max_fix_rounds"],
        "agent" => &["role", "schema", "gate", "on_fail"],
        "sandbox" => &["steps_from", "when"],
        "haruspex_gate" => &["gate", "expect_only"],
        "human" => &["gate", "ask"],
        _ => &[],
    });
    if kind == "learn" {
        k.retain(|x| *x != "effort");
    }
    k
}

fn tier(s: &Stage) -> Result<Tier, String> {
    let m = s
        .model
        .as_deref()
        .ok_or(format!("stage {} needs model", s.id))?;
    Tier::parse(m).ok_or(format!("stage {}: unknown model {m:?}", s.id))
}

fn effort(s: &Stage) -> Result<Option<Effort>, String> {
    match s.effort.as_deref() {
        None => Ok(None),
        Some("low") => Ok(Some(Effort::Low)),
        Some("medium") => Ok(Some(Effort::Medium)),
        Some("high") => Ok(Some(Effort::High)),
        Some(e) => Err(format!("stage {}: unknown effort {e:?}", s.id)),
    }
}

/// The prompt must exist and use only `vars` (and the kernel's own) placeholders (§6.3 rule 6).
fn prompt(a: &Assets, name: Option<&str>, id: &str, vars: &[&str]) -> Result<String, String> {
    let p = name.ok_or(format!("stage {id} needs a prompt"))?;
    let text = a
        .prompt(p)
        .ok_or(format!("stage {id}: no prompt named {p:?}"))?;
    if let Some(bad) = crate::render::placeholders(text)
        .into_iter()
        .find(|v| !vars.contains(&v.as_str()) && !crate::render::KERNEL_VARS.contains(&v.as_str()))
    {
        return Err(format!(
            "stage {id}: prompt {p} uses {{{{{bad}}}}}, which this stage does not provide"
        ));
    }
    Ok(p.to_string())
}

fn llm(a: &Assets, s: &Stage, ceiling_of: &str, vars: &[&str]) -> Result<LlmStage, String> {
    let (turns, secs) = ceiling(ceiling_of);
    Ok(LlmStage {
        tier: tier(s)?,
        effort: effort(s)?,
        prompt: prompt(a, s.prompt.as_deref(), &s.id, vars)?,
        max_turns: s.max_turns.unwrap_or(turns).clamp(1, turns.min(MAX_TURNS)),
        timeout_secs: s.timeout_secs.unwrap_or(secs).clamp(1, secs),
    })
}

fn parse(name: &str, text: &str) -> Result<File, String> {
    let f: File = toml::from_str(text).map_err(|e| format!("pipeline {name}: {e}"))?;
    // Invariant 1: only the compiled-in self pipeline may run without Haruspex (§6.3, §12.2).
    if !f.pipeline.haruspex && name != "self" {
        return Err(format!(
            "pipeline {}: haruspex must be true",
            f.pipeline.name
        ));
    }
    if f.pipeline.name != name {
        return Err(format!(
            "pipeline file {name} names itself {:?}",
            f.pipeline.name
        ));
    }
    for s in &f.stage {
        if let Some(k) = keys_set(s)
            .into_iter()
            .find(|k| !allowed_keys(&s.kind).contains(k))
        {
            return Err(format!(
                "stage {}: key {k} does not apply to {}",
                s.id, s.kind
            ));
        }
    }
    Ok(f)
}

/// The dev pipeline's worst-case calls: read + read!gate + challenge + work and fixes + reviews +
/// review fixes + learn (§5.2).
fn dev_worst(p: &PipelineDef) -> u32 {
    3 + p.max_units * (1 + p.max_fix_rounds)
        + (1 + p.review_max_fix_rounds)
        + p.max_units * p.review_max_fix_rounds
        + 1
}

/// Parses and validates the dev pipeline alone (the in-place chain); `max_calls` is the call cap.
pub fn load(text: &str, max_calls: u32) -> Result<PipelineDef, String> {
    let p = dev(&Assets::compiled(), &parse("dev", text)?)?;
    let worst = dev_worst(&p);
    if worst > max_calls {
        return Err(format!(
            "the pipeline's worst case is {worst} calls, over the cap of {max_calls}"
        ));
    }
    Ok(p)
}

fn dev(a: &Assets, f: &File) -> Result<PipelineDef, String> {
    let kinds: Vec<&str> = f.stage.iter().map(|s| s.kind.as_str()).collect();
    if kinds.iter().copied().ne(ORDER.iter().copied()) {
        return Err(format!(
            "pipeline {}: dev runs exactly the stages {}, in that order",
            f.pipeline.name,
            ORDER.join(", ")
        ));
    }
    let st = |kind: &str| {
        f.stage
            .iter()
            .find(|s| s.kind == kind)
            .expect("order checked")
    };
    let all = crate::render::VARS;
    let (build, review) = (st("build"), st("review"));
    let review_llm = llm(a, review, "review", all)?;
    if review_llm.tier != Tier::Opus {
        return Err("the review stage must use opus".into());
    }
    let max_units = build.max_units.unwrap_or(5);
    let max_fix_rounds = build.max_fix_rounds.unwrap_or(3);
    let review_fix = review.max_fix_rounds.unwrap_or(1);
    if !(1..=8).contains(&max_units) || max_fix_rounds > 5 || review_fix > 1 {
        return Err(
            "limits: max_units 1-8, build max_fix_rounds ≤ 5, review max_fix_rounds ≤ 1".into(),
        );
    }
    let (turns, secs) = ceiling("build");
    Ok(PipelineDef {
        read: llm(a, st("read"), "read", all)?,
        challenge: llm(a, st("challenge"), "challenge", all)?,
        max_units,
        max_fix_rounds,
        build_max_turns: build.max_turns.unwrap_or(turns).clamp(1, turns),
        build_timeout_secs: build.timeout_secs.unwrap_or(secs).clamp(1, secs),
        worker_prompt: prompt(a, build.worker_prompt.as_deref(), &build.id, all)?,
        fix_prompt: prompt(a, build.fix_prompt.as_deref(), &build.id, all)?,
        review: review_llm,
        review_max_fix_rounds: review_fix,
        learn: llm(a, st("learn"), "learn", all)?,
    })
}

/// What a chain's stages may use from the repo's `factory.toml` entry.
#[derive(Clone, Debug, Default)]
pub struct RepoView {
    /// The `qa` check set: names of owner-approved checks (`[[repo]].qa_checks`).
    pub qa_checks: Vec<String>,
    /// `[repo.sandbox].release_rehearsal` has steps.
    pub rehearsal: bool,
}

impl RepoView {
    pub fn of(r: &crate::shift::config::RepoCfg) -> RepoView {
        RepoView {
            qa_checks: r.qa_checks.clone(),
            rehearsal: r
                .sandbox
                .as_ref()
                .is_some_and(|s| !s.release_rehearsal.is_empty()),
        }
    }
}

/// Loads `names` as one chain and checks the invariants over all of it (design §6.3).
pub fn chain(
    a: &Assets,
    names: &[String],
    repo: &RepoView,
    max_calls: u32,
) -> Result<Chain, String> {
    if names.iter().skip(1).any(|n| n == "dev") {
        return Err("dev must come first in a chain".into());
    }
    let mut out = Chain::default();
    let mut ids: Vec<String> = Vec::new();
    for name in names {
        if name == "self" {
            if names.len() != 1 {
                return Err("the self pipeline runs alone".into());
            }
            return self_chain(max_calls);
        }
        let text = a
            .pipeline(name)
            .ok_or_else(|| format!("no pipeline named {name:?}"))?;
        let f = parse(name, text)?;
        for s in &f.stage {
            if ids.contains(&s.id) {
                return Err(format!("stage id {} appears twice in the chain", s.id));
            }
            ids.push(s.id.clone());
        }
        if name == "dev" {
            out.dev = Some(dev(a, &f)?);
            continue;
        }
        let ctx = stages::Ctx {
            assets: a,
            repo,
            built: out.dev.is_some(),
            self_lane: false,
        };
        for s in &f.stage {
            out.stages.push(stages::stage(s, &ctx)?);
        }
        if name == "release" && !f.stage.last().is_some_and(|s| s.kind == "human") {
            return Err("the release pipeline must end with a human stage".into());
        }
    }
    stages::publish_after_review(&out)?;
    let worst = worst_case(&out);
    if worst > max_calls {
        return Err(format!(
            "the chain {}'s worst case is {worst} calls, over the cap of {max_calls}",
            names.join("+")
        ));
    }
    Ok(out)
}

/// The self lane's chain (design §6.2, §12.1): `self.toml` from the compiled assets only (never
/// disk or an overlay, invariant 7), the dev stages with its prompts, then its own publish.
fn self_chain(max_calls: u32) -> Result<Chain, String> {
    let a = Assets::compiled();
    let text = a.pipeline("self").ok_or("no compiled self pipeline")?;
    let File {
        pipeline,
        mut stage,
    } = parse("self", text)?;
    if stage.len() < ORDER.len() {
        return Err("the self pipeline runs the dev stages first".into());
    }
    let rest = stage.split_off(ORDER.len());
    let mut out = Chain {
        dev: Some(dev(&a, &File { pipeline, stage })?),
        ..Chain::default()
    };
    let ctx = stages::Ctx {
        assets: &a,
        repo: &RepoView::default(),
        built: true,
        self_lane: true,
    };
    for s in &rest {
        out.stages.push(stages::stage(s, &ctx)?);
    }
    stages::publish_after_review(&out)?;
    if out.publish().is_none_or(|p| p.draft) {
        return Err("the self pipeline ends with a non-draft publish".into());
    }
    let worst = worst_case(&out);
    if worst > max_calls {
        return Err(format!(
            "the self chain's worst case is {worst} calls, over the cap of {max_calls}"
        ));
    }
    Ok(out)
}

/// The chain's worst-case LLM calls (design §5.2): dev, every fix round of a later stage over all
/// units, every agent run and every review.
pub fn worst_case(c: &Chain) -> u32 {
    let units = c.dev.as_ref().map_or(0, |d| d.max_units);
    let later: u32 = c
        .stages
        .iter()
        .map(|s| match &s.kind {
            StageKind::Checks {
                on_fail: wg_factory::api::OnFail::Fix,
                max_fix_rounds,
                ..
            } => units * max_fix_rounds,
            StageKind::Agent { on_fail, .. } => match on_fail {
                wg_factory::api::OnFail::Fix => 2 + units,
                wg_factory::api::OnFail::Stop => 1,
            },
            StageKind::Review { max_fix_rounds, .. } => 1 + max_fix_rounds + units * max_fix_rounds,
            _ => 0,
        })
        .sum();
    c.dev.as_ref().map_or(0, dev_worst) + later
}

/// The only labels a shift PR may carry. The pipeline is editable and auto-merges (D2), so it never
/// picks a `wg:*` state label or a repo's own label-triggered automation (design §9.5).
pub const PUBLISH_LABELS: [&str; 1] = ["wg:awaiting-approval"];

#[cfg(test)]
mod chain_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::DEV_PIPELINE;

    #[test]
    fn the_shipped_dev_pipeline_loads_with_run_js_limits() {
        let p = load(DEV_PIPELINE, 40).unwrap();
        assert_eq!(
            (p.max_units, p.max_fix_rounds, p.review_max_fix_rounds),
            (5, 3, 1)
        );
        assert_eq!(
            (p.read.tier, p.read.effort),
            (Tier::Opus, Some(Effort::High))
        );
        assert_eq!(p.challenge.tier, Tier::Sonnet);
        assert_eq!((p.learn.tier, p.learn.max_turns), (Tier::Sonnet, 3));
        assert_eq!(p.build_max_turns, 80);
        assert_eq!(dev_worst(&p), 31, "design §5.2: the dev chain's worst case");
    }

    #[test]
    fn each_invariant_rejects_one_bad_pipeline() {
        let cases = [
            (
                DEV_PIPELINE.replace("haruspex = true", "haruspex = false"),
                "haruspex must be true",
            ),
            (
                DEV_PIPELINE.replace("kind = \"escalate\"", "kind = \"publish\""),
                "in that order",
            ),
            (
                DEV_PIPELINE.replace("name = \"dev\"", "name = \"qa\""),
                "names itself",
            ),
            (
                DEV_PIPELINE.replacen(
                    "model = \"opus\"\nprompt = \"review\"",
                    "model = \"sonnet\"\nprompt = \"review\"",
                    1,
                ),
                "must use opus",
            ),
            (
                DEV_PIPELINE.replace("max_units = 5", "max_units = 9"),
                "max_units",
            ),
            (
                DEV_PIPELINE.replace("max_fix_rounds = 1", "max_fix_rounds = 2"),
                "review max_fix_rounds",
            ),
            (
                DEV_PIPELINE.replace("prompt = \"learn\"", "prompt = \"nope\""),
                "no prompt named",
            ),
            (
                DEV_PIPELINE.replace(
                    "effort = \"high\"\nprompt = \"read\"",
                    "effort = \"max\"\nprompt = \"read\"",
                ),
                "unknown effort",
            ),
            (
                DEV_PIPELINE.replace(
                    "kind = \"baseline\"",
                    "kind = \"baseline\"\nmodel = \"opus\"",
                ),
                "does not apply",
            ),
            (format!("{DEV_PIPELINE}\nsurprise = 1\n"), "unknown field"),
            (
                format!("{DEV_PIPELINE}\n[[stage]]\nid = \"publish\"\nkind = \"publish\"\n"),
                "in that order",
            ),
        ];
        for (text, want) in cases {
            let err = load(&text, 40).unwrap_err();
            assert!(err.contains(want), "{want}: {err}");
        }
        assert!(load(DEV_PIPELINE, 20)
            .unwrap_err()
            .contains("worst case is 31"));
        let clamped = load(
            &DEV_PIPELINE.replace(
                "max_turns = 60\ntimeout_secs = 1800\n\n[[stage]]\nid = \"challenge\"",
                "max_turns = 900\ntimeout_secs = 1800\n\n[[stage]]\nid = \"challenge\"",
            ),
            40,
        )
        .unwrap();
        assert_eq!(clamped.read.max_turns, 60);
    }
}
