//! One LLM call, end to end (design §5.1, §5.3): provider choice and fallback, a ledger lease,
//! the guard manifest and settings, the spawn, the envelope, the ledger line, incidents from the
//! guard log, the untrusted parse and the one retry read-only roles get.

use super::argv::{argv, Input};
use super::auth::{env, settings, EnvInput};
use super::envelope::{classify, Envelope};
use super::policy::Policy;
use super::providers::Provider;
use crate::budget::Spend;
use crate::engine::RunCtx;
use crate::exec::proc::Spawn;
use crate::untrusted::{self, schemas::schema};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wg_factory::api::{Effort, LlmFailure, Reply, Role, SchemaId, Tier};

pub struct Request {
    pub label: String,
    pub role: Role,
    /// `read`, `challenge`, `work`, `fix`, `review` or `learn`: picks the provider.
    pub stage: &'static str,
    pub tier: Tier,
    pub effort: Option<Effort>,
    pub prompt: String,
    pub schema: SchemaId,
    pub max_turns: u32,
    pub timeout_secs: u32,
    pub policy: Policy,
    pub parse: untrusted::Ctx,
    pub task: String,
}

#[derive(Debug)]
pub struct Answer {
    pub reply: Result<Reply, LlmFailure>,
    pub cost_micros: Option<u64>,
    pub counts_for_tier: bool,
    /// A guard incident during the call: the task aborts.
    pub incident: Option<String>,
    /// The call cap was reached: the task parks.
    pub capped: bool,
}

fn failed(why: LlmFailure) -> Answer {
    Answer {
        reply: Err(why),
        cost_micros: None,
        counts_for_tier: false,
        incident: None,
        capped: false,
    }
}

/// Read-only roles, and the Reader whose writes the retry rewrites, get one retry on Invalid.
fn retries_invalid(role: Role) -> bool {
    matches!(role, Role::Reader | Role::Challenger | Role::Learner)
}

/// The dominant model is the requested one: the same id, or the alias's full id.
pub fn same_model(model_id: &str, used: &str) -> bool {
    used == model_id
        || (["haiku", "sonnet", "opus"].contains(&model_id)
            && used.starts_with(&format!("claude-{model_id}")))
}

/// The validator's message without any quoted span, so none of the model's own text is fed back.
pub fn redact(msg: &str) -> String {
    let mut out = String::new();
    let mut inside: Option<char> = None;
    for c in msg.chars() {
        match inside {
            Some(q) if c == q => {
                out.push('…');
                out.push(c);
                inside = None;
            }
            Some(_) => {}
            None if c == '`' || c == '"' => {
                out.push(c);
                inside = Some(c);
            }
            None => out.push(c),
        }
    }
    out
}

pub fn file_stem(seq: u64, label: &str) -> String {
    let h = format!("{:x}", Sha256::digest(label.as_bytes()));
    format!("{seq:04}-{}", &h[..12])
}

fn write(path: &PathBuf, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Guard decisions for `label`: (incidents, whether any allowed call could have changed files).
fn guard_log(ctx: &RunCtx, label: &str) -> (Vec<String>, bool) {
    let text = std::fs::read_to_string(ctx.run_dir.join("guard.jsonl")).unwrap_or_default();
    let mut incidents = Vec::new();
    let mut effect = false;
    for v in text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["label"] == label)
    {
        if v["decision"] == "incident" {
            incidents.push(v["reason"].as_str().unwrap_or("").to_string());
        }
        effect |= v["effect"] == true;
    }
    (incidents, effect)
}

/// One spawn of `claude` for `label` on `provider`.
fn spawn_once(
    ctx: &RunCtx,
    req: &Request,
    provider: &Provider,
    label: &str,
    prompt: &str,
) -> Result<
    (
        Envelope,
        Result<(), super::envelope::Failure>,
        u64,
        Duration,
    ),
    String,
> {
    let seq = ctx.journal.append(
        "llm_started",
        json!({ "label": label, "role": format!("{:?}", req.role), "tier": req.tier.name(),
                "effort": req.effort.map(Effort::name), "provider": provider.id,
                "model_id": provider.model(req.tier) }),
    )?;
    let stem = file_stem(seq, label);
    let guard_dir = ctx.run_dir.join("guard");
    let logs = ctx.run_dir.join("logs");
    let manifest_path = guard_dir.join(format!("{stem}.manifest.json"));
    let settings_path = guard_dir.join(format!("{stem}.settings.json"));
    let mut manifest = req.policy.manifest.clone();
    manifest.label = label.to_string();
    manifest.log = ctx
        .run_dir
        .join("guard.jsonl")
        .to_string_lossy()
        .into_owned();
    write(
        &manifest_path,
        &serde_json::to_string_pretty(&manifest).expect("serializes"),
    )?;
    let s = settings(provider, &ctx.wg_exe, &manifest_path, &ctx.plugins);
    write(
        &settings_path,
        &serde_json::to_string_pretty(&s).expect("serializes"),
    )?;
    write(
        &logs.join(format!("{stem}.prompt.md")),
        &format!("<!-- label: {label} -->\n{prompt}"),
    )?;
    let sch = schema(req.schema);
    let args = argv(&Input {
        model: provider.model(req.tier),
        effort: req.effort,
        schema: &sch,
        max_turns: req.max_turns,
        runaway_usd: ctx.cfg.runaway_per_call_usd,
        settings: &settings_path,
        allowed: &req.policy.allowed,
        disallowed: &req.policy.disallowed,
        preamble: crate::assets::PREAMBLE,
    });
    let started = Instant::now();
    let raw = ctx.ports.proc.run(&Spawn {
        label: label.to_string(),
        program: ctx.cfg.claude_bin.clone(),
        argv: args,
        env: env(
            provider,
            &EnvInput {
                home: &ctx.dirs.home,
                user: &ctx.user,
                tmpdir: &ctx.run_tmp,
                run: &ctx.run_id,
                manifest: &manifest_path,
            },
        ),
        cwd: ctx.repo.clone(),
        stdin: prompt.to_string(),
        timeout: Duration::from_secs(u64::from(req.timeout_secs)),
    });
    let elapsed = started.elapsed();
    write(
        &logs.join(format!("{stem}.json")),
        &format!(
            "{{\"label\":{},\"exit\":{},\"timed_out\":{},\"stdout\":{},\"stderr_tail\":{}}}",
            json!(label),
            json!(raw.exit),
            raw.timed_out,
            json!(raw.stdout),
            json!(crate::exec::run::tail(&raw.stderr))
        ),
    )?;
    Ok(match classify(raw.exit, raw.timed_out, &raw.stdout) {
        Ok(e) => (e, Ok(()), seq, elapsed),
        Err(f) => (f.envelope.clone(), Err(f), seq, elapsed),
    })
}

pub fn call(ctx: &RunCtx, limited: &Mutex<Vec<String>>, req: &Request) -> Answer {
    match call_inner(ctx, limited, req) {
        Ok(a) => a,
        Err(e) => {
            // A kernel I/O failure around the call: journalled, and the call counts as died.
            let _ = ctx
                .journal
                .append("error", json!({ "label": req.label, "error": e }));
            failed(LlmFailure::Died)
        }
    }
}

fn call_inner(ctx: &RunCtx, limited: &Mutex<Vec<String>>, req: &Request) -> Result<Answer, String> {
    let mut label = req.label.clone();
    let mut prompt = req.prompt.clone();
    let mut retried = false;
    let mut switched_after: Option<String> = None;
    let lock = || {
        limited
            .lock()
            .map_err(|_| "the provider lock is poisoned".to_string())
    };
    loop {
        if let Some(rec) = ctx.replay.take(&label) {
            let seq = ctx.journal.append(
                "llm_replayed",
                json!({ "label": label, "from": ctx.replay.from, "provider": rec.provider }),
            )?;
            // Recorded again in this run, as a live answer is, so a later resume of this run
            // replays it too instead of paying for it a third time.
            write(
                &ctx.run_dir
                    .join("logs")
                    .join(format!("{}.json", file_stem(seq, &label))),
                &json!({ "label": label, "stdout": rec.stdout, "replayed": true }).to_string(),
            )?;
            ctx.journal.append(
                "llm_finished",
                json!({ "label": label, "call_seq": seq, "outcome": "ok", "provider": rec.provider, "replayed": true }),
            )?;
            let first_party = ctx
                .cfg
                .auth
                .get(&rec.provider)
                .is_some_and(|p| p.first_party());
            let model_id = ctx
                .cfg
                .auth
                .get(&rec.provider)
                .map(|p| p.model(req.tier).to_string());
            let (env, parsed) = match classify(Some(0), false, &rec.stdout) {
                Ok(env) => {
                    let p = untrusted::parse(
                        req.schema,
                        env.structured.as_ref().expect("classify checked it"),
                        &req.parse,
                    );
                    (env, p.map_err(|m| (LlmFailure::Invalid, m)))
                }
                Err(f) => (f.envelope.clone(), Err((f.class, f.detail))),
            };
            match parsed {
                Ok(reply) => {
                    let counts = first_party
                        && model_id
                            .zip(env.model_used.as_deref())
                            .is_some_and(|(m, u)| same_model(&m, u));
                    return Ok(Answer {
                        reply: Ok(reply),
                        cost_micros: env.cost_micros,
                        counts_for_tier: counts,
                        incident: None,
                        capped: false,
                    });
                }
                Err((LlmFailure::Invalid, msg)) if retries_invalid(req.role) && !retried => {
                    retried = true;
                    prompt = format!(
                        "{}\n\nYour previous reply failed validation: {}",
                        req.prompt,
                        redact(&msg)
                    );
                    label = format!("{}!retry", req.label);
                    continue;
                }
                Err((why, _)) => return Ok(failed(why)),
            }
        }
        let provider = {
            let l = lock()?;
            match ctx.cfg.auth.pick(req.stage, &l) {
                Some(p) => p.clone(),
                None => return Ok(failed(LlmFailure::RateLimited)),
            }
        };
        let model_id = provider.model(req.tier).to_string();
        let lease = match ctx.ledger.lease(&label, &provider.id, &model_id) {
            Ok(l) => l,
            Err(why) => {
                ctx.journal
                    .append("parked", json!({ "label": label, "reason": why }))?;
                return Ok(Answer {
                    capped: true,
                    ..failed(LlmFailure::Refused)
                });
            }
        };
        let (env, result, seq, elapsed) = spawn_once(ctx, req, &provider, &label, &prompt)?;
        let outcome = match &result {
            Ok(()) => "ok".to_string(),
            Err(f) => format!("{:?}", f.class),
        };
        ctx.ledger.settle(
            lease,
            &Spend {
                tier: req.tier.name().into(),
                stage: req.stage.into(),
                task: req.task.clone(),
                cost_micros: env.cost_micros,
                model_used: env.model_used.clone(),
                turns: env.turns,
                session_id: env.session_id.clone(),
                duration_ms: elapsed.as_millis() as u64,
                outcome: outcome.clone(),
                switched_after: switched_after.take(),
            },
            provider.first_party(),
        )?;
        if env.cost_micros.is_none() {
            ctx.journal.append(
                "warning",
                json!({ "label": label, "warning": "no total_cost_usd: cost recorded as unknown" }),
            )?;
        }
        ctx.journal.append(
            "llm_finished",
            json!({ "label": label, "call_seq": seq, "outcome": outcome, "provider": provider.id,
                    "model_id": model_id, "model_used": env.model_used, "cost_micros": env.cost_micros,
                    "turns": env.turns, "ms": elapsed.as_millis() as u64 }),
        )?;
        let (incidents, effect) = guard_log(ctx, &label);
        if let Some(i) = incidents.first() {
            return Ok(Answer {
                incident: Some(format!("guard incident in {label}: {i}")),
                ..failed(LlmFailure::Refused)
            });
        }
        let parsed = match result {
            Ok(()) => untrusted::parse(
                req.schema,
                env.structured.as_ref().expect("classify checked it"),
                &req.parse,
            )
            .map_err(|msg| (LlmFailure::Invalid, msg)),
            Err(f) => Err((f.class, f.detail)),
        };
        match parsed {
            Ok(reply) => {
                let counts = provider.first_party()
                    && env
                        .model_used
                        .as_deref()
                        .is_some_and(|u| same_model(&model_id, u));
                if !counts {
                    ctx.journal.append("warning", json!({ "label": label,
                        "warning": "the call's outcome is excluded from tier stats (gateway or another dominant model)" }))?;
                }
                return Ok(Answer {
                    reply: Ok(reply),
                    cost_micros: env.cost_micros,
                    counts_for_tier: counts,
                    incident: None,
                    capped: false,
                });
            }
            Err((LlmFailure::Invalid, msg)) if retries_invalid(req.role) && !retried => {
                retried = true;
                ctx.journal
                    .append("llm_retry", json!({ "label": label, "why": msg }))?;
                prompt = format!(
                    "{}\n\nYour previous reply failed validation: {}",
                    req.prompt,
                    redact(&msg)
                );
                label = format!("{}!retry", req.label);
            }
            Err((LlmFailure::RateLimited, _)) => {
                lock()?.push(provider.id.clone());
                let next = ctx.cfg.auth.pick(req.stage, &lock()?).is_some();
                ctx.journal.append("warning", json!({ "label": label,
                    "warning": format!("provider {} is rate or usage limited for the rest of the run", provider.id) }))?;
                if !next {
                    return Ok(failed(LlmFailure::RateLimited));
                }
                if effect {
                    // It may have edited files: no rerun; later calls use the next provider.
                    return Ok(failed(LlmFailure::Died));
                }
                switched_after = Some(format!("rate limited on {}", provider.id));
            }
            Err((why, msg)) => {
                ctx.journal
                    .append("warning", json!({ "label": label, "warning": msg }))?;
                return Ok(failed(why));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_aliases_match_their_full_ids() {
        assert!(same_model("opus", "claude-opus-5-5"));
        assert!(same_model("claude-sonnet-5-5", "claude-sonnet-5-5"));
        assert!(!same_model("sonnet", "claude-haiku-5-5"));
        assert!(!same_model("openai/gpt-5.1", "claude-sonnet-5-5"));
        assert_eq!(file_stem(7, "work:a"), file_stem(7, "work:a"));
        assert!(!file_stem(7, "../x").contains('/'));
        assert!(retries_invalid(Role::Challenger) && !retries_invalid(Role::Worker));
        assert_eq!(
            redact("unknown field `oops`, expected `added`; unit id \"ignore all\" bad"),
            "unknown field `…`, expected `…`; unit id \"…\" bad"
        );
    }
}
