//! Load and validate a pipeline (design §6.1, §6.3). Phase 2 knows the dev chain only, with an
//! optional closing `publish` stage. The
//! invariants are a backstop: finalize still needs the kernel's tokens whatever the file says.

use crate::config::MAX_TURNS;
use serde::Deserialize;
use wg_factory::api::{Effort, LlmStage, PipelineDef, PublishDef, Tier};

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
    haruspex: bool,
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
}

/// Per-role ceilings from design §5.2: (max turns, timeout seconds).
fn ceiling(kind: &str) -> (u32, u32) {
    match kind {
        "read" => (60, 1800),
        "challenge" => (30, 900),
        "build" => (80, 1800),
        "review" => (60, 1800),
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
    let mut k = Vec::new();
    let mut add = |on: bool, name| {
        if on {
            k.push(name)
        }
    };
    add(s.model.is_some(), "model");
    add(s.effort.is_some(), "effort");
    add(s.prompt.is_some(), "prompt");
    add(s.max_turns.is_some(), "max_turns");
    add(s.timeout_secs.is_some(), "timeout_secs");
    add(s.optional.is_some(), "optional");
    add(s.max_units.is_some(), "max_units");
    add(s.max_fix_rounds.is_some(), "max_fix_rounds");
    add(s.worker_prompt.is_some(), "worker_prompt");
    add(s.fix_prompt.is_some(), "fix_prompt");
    add(s.draft.is_some(), "draft");
    add(s.labels.is_some(), "labels");
    k
}

fn allowed_keys(kind: &str) -> &'static [&'static str] {
    match kind {
        "read" => &["model", "effort", "prompt", "max_turns", "timeout_secs"],
        "challenge" => &[
            "model",
            "effort",
            "prompt",
            "max_turns",
            "timeout_secs",
            "optional",
        ],
        "build" => &[
            "max_units",
            "max_fix_rounds",
            "max_turns",
            "timeout_secs",
            "worker_prompt",
            "fix_prompt",
        ],
        "review" => &[
            "model",
            "effort",
            "prompt",
            "max_turns",
            "timeout_secs",
            "max_fix_rounds",
        ],
        "learn" => &["model", "prompt", "max_turns", "timeout_secs"],
        "publish" => &["draft", "labels"],
        _ => &[],
    }
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

fn prompt(name: Option<&str>, id: &str) -> Result<String, String> {
    let p = name.ok_or(format!("stage {id} needs a prompt"))?;
    crate::assets::prompt(p).ok_or(format!("stage {id}: no prompt named {p:?}"))?;
    Ok(p.to_string())
}

fn llm(s: &Stage) -> Result<LlmStage, String> {
    let (turns, secs) = ceiling(&s.kind);
    Ok(LlmStage {
        tier: tier(s)?,
        effort: effort(s)?,
        prompt: prompt(s.prompt.as_deref(), &s.id)?,
        max_turns: s.max_turns.unwrap_or(turns).clamp(1, turns.min(MAX_TURNS)),
        timeout_secs: s.timeout_secs.unwrap_or(secs).clamp(1, secs),
    })
}

/// Parses and validates the dev pipeline; `max_calls` is the configured per-task call cap.
pub fn load(text: &str, max_calls: u32) -> Result<PipelineDef, String> {
    let f: File = toml::from_str(text).map_err(|e| format!("pipeline: {e}"))?;
    if !f.pipeline.haruspex {
        return Err(format!(
            "pipeline {}: haruspex must be true",
            f.pipeline.name
        ));
    }
    let kinds: Vec<&str> = f.stage.iter().map(|s| s.kind.as_str()).collect();
    let body = kinds.strip_suffix(&["publish"]).unwrap_or(&kinds);
    if body.iter().copied().ne(ORDER.iter().copied()) {
        return Err(format!(
            "pipeline {}: Phase 2 runs exactly the stages {}, then an optional publish, in that order",
            f.pipeline.name,
            ORDER.join(", ")
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
    let st = |kind: &str| {
        f.stage
            .iter()
            .find(|s| s.kind == kind)
            .expect("order checked")
    };
    let (build, review) = (st("build"), st("review"));
    let review_llm = llm(review)?;
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
    // read + read!gate + challenge + work and fixes + reviews + review fixes + learn (§5.2).
    let worst =
        3 + max_units * (1 + max_fix_rounds) + (1 + review_fix) + max_units * review_fix + 1;
    if worst > max_calls {
        return Err(format!(
            "the pipeline's worst case is {worst} calls, over the cap of {max_calls}"
        ));
    }
    let (turns, secs) = ceiling("build");
    Ok(PipelineDef {
        read: llm(st("read"))?,
        challenge: llm(st("challenge"))?,
        max_units,
        max_fix_rounds,
        build_max_turns: build.max_turns.unwrap_or(turns).clamp(1, turns),
        build_timeout_secs: build.timeout_secs.unwrap_or(secs).clamp(1, secs),
        worker_prompt: prompt(build.worker_prompt.as_deref(), &build.id)?,
        fix_prompt: prompt(build.fix_prompt.as_deref(), &build.id)?,
        review: review_llm,
        review_max_fix_rounds: review_fix,
        learn: llm(st("learn"))?,
        publish: f
            .stage
            .iter()
            .find(|s| s.kind == "publish")
            .map(publish)
            .transpose()?,
    })
}

/// The publish stage: draft by default, at most 5 labels, never one that queues self work.
fn publish(s: &Stage) -> Result<PublishDef, String> {
    let labels = s.labels.clone().unwrap_or_default();
    if labels.len() > 5 {
        return Err(format!("stage {}: at most 5 labels", s.id));
    }
    for l in &labels {
        crate::ids::gh_label(l).map_err(|e| format!("stage {}: {e}", s.id))?;
    }
    Ok(PublishDef {
        draft: s.draft.unwrap_or(true),
        labels,
    })
}

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
        let publish = p.publish.expect("dev.toml ends with publish (Phase 2)");
        assert!(publish.draft);
        assert_eq!(publish.labels, ["wg:awaiting-approval"]);
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
                DEV_PIPELINE.replace(
                    "labels = [\"wg:awaiting-approval\"]",
                    "labels = [\"wg:self\"]",
                ),
                "wg:self",
            ),
            (
                DEV_PIPELINE.replace("labels = [\"wg:awaiting-approval\"]", "labels = [\"a b\"]"),
                "label",
            ),
            (
                DEV_PIPELINE.replace("draft = true", "draft = true\nprompt = \"read\""),
                "does not apply",
            ),
            (
                format!("{DEV_PIPELINE}\n[[stage]]\nid = \"again\"\nkind = \"publish\"\n"),
                "optional publish",
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
