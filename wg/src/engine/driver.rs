//! The step loop: feed an event to the factory, validate each command it returns, execute it, turn
//! the result into the next event. Events are fed in the order the commands were issued, so runs
//! are deterministic even though one layer's calls run in parallel.

use super::command_check::{self, Refusal};
use super::kernel::{Kernel, PENDING_APPROVAL};
use super::RunCtx;
use crate::claude::runner::{self, Answer, Request};
use crate::memory_store::{self, Loaded};
use crate::tokens::ReviewPass;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use wg_factory::api::{
    Chain, Command, Decision, Escalation, Event, LlmCall, LlmFailure, Outcome, Reply, Role,
    RunInput, Tier,
};
use wg_factory::model::RunState;

pub type Step<'f> = &'f dyn Fn(&RunState, &Event) -> (RunState, Vec<Command>);

pub struct Start {
    pub task: String,
    pub title: String,
    pub decisions: Vec<Decision>,
    pub resume_task: Option<String>,
    pub initialized_earlier: bool,
    pub user_changes_before: Vec<String>,
    pub memory: Loaded,
    pub memory_path: PathBuf,
    /// The repo's chain: dev and its later stages (a shift), dev alone (in place), or the release
    /// line alone.
    pub chain: Chain,
    pub start_branch: String,
    pub initializing: bool,
    /// Checks that failed at the first clean baseline of this task (a rerun on a kept worktree):
    /// only these may leave the gate, so an earlier run's edits never shrink it (design §4.1).
    pub baseline_floor: Option<Vec<String>>,
    /// An earlier run of this task built a risky unit on the worktree this run keeps: the task is
    /// tainted from the start, so even its baseline runs in the sandbox (design §8.1).
    pub tainted_earlier: bool,
}

/// How a run ended. Made once per run.
#[allow(clippy::large_enum_variant)]
pub enum End {
    Finish(Outcome),
    NeedsDecision(Vec<Escalation>),
    Incident(String),
    Parked(String),
    Failed(String),
    /// A human stage: the run waits for the owner at `gate` (Phase 4). Nothing was approved.
    AwaitingOwner {
        gate: String,
        ask: String,
    },
}

pub struct Ran {
    pub end: End,
    /// The draft PR's URL, when publish succeeded.
    pub published: Option<String>,
    pub kernel: Kernel,
    pub memory_saved: bool,
    pub rejected_rules: Vec<(String, String)>,
}

struct Loop<'a> {
    ctx: &'a RunCtx<'a>,
    k: Kernel,
    limited: Mutex<Vec<String>>,
    queue: VecDeque<Event>,
    max_units: usize,
    memory_path: PathBuf,
    raw_models: Value,
    can_save: bool,
    memory_saved: bool,
    rejected: Vec<(String, String)>,
    publish: Option<wg_factory::api::PublishDef>,
    published: Option<String>,
    /// The chain's later stages: a gate, sandbox or human command runs only if one declares it.
    stages: Vec<wg_factory::api::StageDef>,
}

pub fn run_task(ctx: &RunCtx, start: Start, step: Step) -> Result<Ran, String> {
    let mut k = Kernel {
        decided: start.decisions.iter().map(|d| d.id.clone()).collect(),
        resume_task: start.resume_task.clone(),
        user_changes: start.user_changes_before.clone(),
        initializing: start.initializing,
        start_branch: start.start_branch.clone(),
        baseline_floor: start.baseline_floor.clone(),
        approvals: ctx.ports.hx.approvals()?,
        managed: ctx.ports.hx.managed_hash()?,
        tainted: start.tainted_earlier
            || ctx
                .cfg
                .repo(&ctx.repo_name)
                .is_some_and(|r| r.always_tainted),
        qa: start
            .chain
            .stages
            .iter()
            .flat_map(|s| match &s.kind {
                wg_factory::api::StageKind::Checks { names, .. } => names.clone(),
                _ => vec![],
            })
            .collect(),
        ..Kernel::default()
    };
    // The release line has no Reading: the contract it may not weaken is the task as merged.
    if start.chain.dev.is_none() {
        if let Some(t) = &start.resume_task {
            k.contract = Some(crate::haruspex::contract::snapshot(&ctx.ports.hx.task(t)?));
        }
    }
    for w in &start.memory.warnings {
        ctx.journal.append("warning", json!({ "warning": w }))?;
    }
    let input = RunInput {
        task: start.task.clone(),
        title: start.title.clone(),
        decisions: start.decisions.clone(),
        resume_task: start.resume_task.clone(),
        initialized_earlier: start.initialized_earlier,
        user_changes_before: start.user_changes_before.clone(),
        memory: start.memory.memory.clone(),
        can_save: start.memory.can_save,
        chain: start.chain.clone(),
    };
    let mut l = Loop {
        ctx,
        k,
        limited: Mutex::new(Vec::new()),
        queue: VecDeque::from([Event::Started(input)]),
        max_units: start.chain.dev.as_ref().map_or(0, |d| d.max_units as usize),
        memory_path: start.memory_path,
        raw_models: start.memory.raw_models,
        can_save: start.memory.can_save,
        memory_saved: false,
        rejected: Vec::new(),
        publish: start.chain.publish().cloned(),
        published: None,
        stages: start.chain.stages.clone(),
    };
    let mut state = RunState::default();
    let end = loop {
        let Some(ev) = l.queue.pop_front() else {
            break End::Failed("the factory stopped without finishing".into());
        };
        // A publish's answer is always fed: once the draft PR exists the task is a PR, whatever
        // stops the shift now.
        let answering_publish =
            matches!(ev, Event::Published { .. } | Event::PublishRefused { .. });
        let stop = ctx.shift.as_ref().filter(|_| !answering_publish);
        if let Some(why) = stop.and_then(|s| (s.stop)()) {
            ctx.journal.append("parked", json!({ "reason": why }))?;
            break End::Parked(format!("shift stopped: {why}"));
        }
        ctx.journal
            .append("step", json!({ "event": event_name(&ev) }))?;
        let (next, cmds) = step(&state, &ev);
        state = next;
        if let Some(end) = l.execute(cmds)? {
            break end;
        }
    };
    Ok(Ran {
        end,
        published: l.published,
        kernel: l.k,
        memory_saved: l.memory_saved,
        rejected_rules: l.rejected,
    })
}

fn event_name(ev: &Event) -> String {
    match ev {
        Event::Started(_) => "started".into(),
        Event::LlmDone { label, .. } => format!("llm_done {label}"),
        Event::LlmFailed { label, why } => format!("llm_failed {label} {why:?}"),
        Event::ChecksDone { label, .. } => format!("checks_done {label}"),
        Event::ChecksUnavailable { label, .. } => format!("checks_unavailable {label}"),
        Event::RiskUpdated(_) => "risk_updated".into(),
        Event::KernelEscalations(e) => format!("kernel_escalations {}", e.len()),
        Event::Published { .. } => "published".into(),
        Event::PublishRefused { .. } => "publish_refused".into(),
        Event::GateDone { label, .. } => format!("gate_done {label}"),
        Event::SandboxDone { label, .. } => format!("sandbox_done {label}"),
    }
}

/// The factory's escalations, then the kernel's open ones it left out.
fn merge(factory: &[Escalation], kernel: Vec<Escalation>) -> Vec<Escalation> {
    let mut out = factory.to_vec();
    for e in kernel {
        if !out.iter().any(|x| x.id == e.id) {
            out.push(e);
        }
    }
    out
}

impl Loop<'_> {
    fn execute(&mut self, cmds: Vec<Command>) -> Result<Option<End>, String> {
        let mut batch: Vec<LlmCall> = Vec::new();
        for cmd in cmds {
            if let Command::Llm(c) = cmd {
                batch.push(c);
                continue;
            }
            if let Some(end) = self.flush(std::mem::take(&mut batch))? {
                return Ok(Some(end));
            }
            if let Some(end) = self.one(cmd)? {
                return Ok(Some(end));
            }
        }
        self.flush(batch)
    }

    /// Whether the chain declares the stage a gate, sandbox or human command stands for.
    fn declared(&self, cmd: &Command) -> bool {
        use wg_factory::api::StageKind as K;
        self.stages.iter().any(|s| match (cmd, &s.kind) {
            (Command::Gate { gate, .. }, K::HaruspexGate { gate: g, .. }) => gate == g,
            (Command::Sandbox { steps_from, .. }, K::Sandbox { steps_from: f, .. }) => {
                steps_from == f
            }
            (Command::Human { gate, .. }, K::Human { gate: g, .. }) => gate == g,
            _ => false,
        })
    }

    fn one(&mut self, cmd: Command) -> Result<Option<End>, String> {
        let j = &self.ctx.journal;
        if matches!(
            cmd,
            Command::Gate { .. } | Command::Sandbox { .. } | Command::Human { .. }
        ) && !self.declared(&cmd)
        {
            j.append(
                "warning",
                json!({ "refused": "a gate, sandbox or human command the chain declares no stage for" }),
            )?;
            return Ok(Some(End::Failed(
                "refused: the factory asked for a stage this chain does not have".into(),
            )));
        }
        match cmd {
            Command::Llm(_) => unreachable!("batched by execute"),
            Command::Checks { label, names } => {
                j.append("command", json!({ "label": label, "what": "checks" }))?;
                self.checks(&label, &names)
            }
            Command::Escalate(list) => {
                j.append("command", json!({ "what": "escalate" }))?;
                Ok(Some(End::NeedsDecision(merge(&list, self.k.open()))))
            }
            Command::SaveMemory(m) => {
                if !self.can_save {
                    j.append(
                        "warning",
                        json!({ "warning": "SaveMemory refused: this run may not save memory" }),
                    )?;
                    return Ok(None);
                }
                match memory_store::save(&self.memory_path, &m, &self.raw_models) {
                    Ok(rejected) => {
                        for (id, why) in &rejected {
                            j.append("rule_rejected", json!({ "rule": id, "why": why }))?;
                        }
                        j.append("memory_saved", json!({ "path": self.memory_path }))?;
                        self.rejected = rejected;
                        self.memory_saved = true;
                    }
                    Err(e) => {
                        j.append(
                            "error",
                            json!({ "error": format!("memory not saved: {e}") }),
                        )?;
                    }
                }
                Ok(None)
            }
            Command::Log(s) => {
                j.append(
                    "warning",
                    json!({ "warning": crate::ids::one_line(&s, 1000) }),
                )?;
                Ok(None)
            }
            Command::Publish { title } => {
                j.append("command", json!({ "what": "publish" }))?;
                let r = match &self.publish {
                    Some(def) => super::publish::publish(self.ctx, &self.k, def, &title),
                    None => Err("the pipeline has no publish stage".into()),
                };
                match r {
                    Ok(url) => {
                        j.append("pr_opened", json!({ "url": url }))?;
                        self.published = Some(url.clone());
                        self.queue.push_back(Event::Published { url });
                    }
                    Err(why) if why.starts_with("incident") => {
                        j.append("incident", json!({ "at": "publish", "why": why }))?;
                        return Ok(Some(End::Incident(why)));
                    }
                    Err(why) => {
                        j.append("warning", json!({ "refused": format!("publish: {why}") }))?;
                        self.queue.push_back(Event::PublishRefused { why });
                    }
                }
                Ok(None)
            }
            Command::Gate { label, gate } => {
                j.append("command", json!({ "label": label, "what": "gate" }))?;
                if let Err(r) = command_check::gate(&label, &gate) {
                    j.append("warning", json!({ "refused": r.why }))?;
                    return Ok(Some(End::Failed(format!("gate refused: {}", r.why))));
                }
                let blockers = self.ctx.ports.hx.gate(&gate)?;
                j.append(
                    "gate_evaluated",
                    json!({ "gate": gate, "label": label, "blockers": blockers }),
                )?;
                self.queue.push_back(Event::GateDone { label, blockers });
                Ok(None)
            }
            Command::Sandbox { label, steps_from } => {
                j.append("command", json!({ "label": label, "what": "sandbox" }))?;
                super::stage_exec::sandbox(
                    self.ctx,
                    &mut self.k,
                    &mut self.queue,
                    label,
                    &steps_from,
                )
            }
            Command::Human { gate, ask } => {
                if !crate::pipelines::GATES.contains(&gate.as_str()) {
                    return Ok(Some(End::Failed(
                        "a human stage names an unknown gate".into(),
                    )));
                }
                // The owner is asked on the kernel's evidence, never on the factory's word (§9.5).
                if let Some(why) =
                    super::stage_exec::human_refusal(self.ctx, &self.k, &self.stages, &gate)?
                {
                    j.append("warning", json!({ "refused": format!("human: {why}") }))?;
                    return Ok(Some(End::Failed(format!("refused: {why}"))));
                }
                let ask = crate::ids::one_line(&ask, 300);
                j.append("parked", json!({ "human": gate, "ask": ask }))?;
                Ok(Some(End::AwaitingOwner { gate, ask }))
            }
            Command::Finish(o) => Ok(Some(End::Finish(o))),
        }
    }

    fn checks(&mut self, label: &str, names: &[String]) -> Result<Option<End>, String> {
        let qa = command_check::qa_set(self.ctx);
        let names = match command_check::checks(&self.k, label, names, &qa) {
            Ok(n) => n,
            Err(r) => {
                self.ctx
                    .journal
                    .append("warning", json!({ "label": label, "refused": r.why }))?;
                self.queue.push_back(Event::ChecksUnavailable {
                    label: label.into(),
                    why: r.why,
                });
                return Ok(None);
            }
        };
        // Before the baseline too: a kept worktree may already hold a risky unit's edits.
        let risky_before = (self.k.risky.len(), self.k.tainted);
        if let Some(end) = super::risk::after_layer(self.ctx, &mut self.k)? {
            return Ok(Some(end));
        }
        if label != "check:baseline" || (self.k.risky.len(), self.k.tainted) != risky_before {
            self.queue
                .push_back(Event::RiskUpdated(self.k.risk_view(self.ctx)));
        }
        match crate::checks::run(self.ctx, label, &names, self.k.tainted) {
            Err(why) if why.starts_with("sandbox unavailable") => {
                self.ctx
                    .journal
                    .append("parked", json!({ "label": label, "reason": why }))?;
                return Ok(Some(End::Parked(why)));
            }
            Err(why) => {
                self.ctx
                    .journal
                    .append("error", json!({ "label": label, "error": why }))?;
                self.queue.push_back(Event::ChecksUnavailable {
                    label: label.into(),
                    why,
                });
            }
            Ok((results, changed, evidence)) => {
                self.k.sandbox_runs.extend(evidence);
                if label == "check:baseline" {
                    self.k.record_baseline(&results);
                    if self.k.gate.is_empty() {
                        return Ok(Some(End::NeedsDecision(vec![self.k.baseline_escalation()])));
                    }
                } else {
                    for r in results.iter().filter(|r| !r.passed()) {
                        let sig =
                            wg_factory::plan::signature::signature(r.command(), r.output_tail());
                        if !self.k.seen_sigs.contains(&sig) {
                            self.k.seen_sigs.push(sig);
                        }
                    }
                }
                let outcomes = crate::checks::outcomes(&results);
                self.k.checks.extend(results);
                self.queue.push_back(Event::ChecksDone {
                    label: label.into(),
                    results: outcomes,
                    changed_files: changed,
                });
            }
        }
        Ok(None)
    }

    /// Validates a batch, runs the valid calls in parallel, and feeds the answers in issue order.
    fn flush(&mut self, batch: Vec<LlmCall>) -> Result<Option<End>, String> {
        if batch.is_empty() {
            return Ok(None);
        }
        // The batch runs at once in one worktree: if it builds a risky unit, every call in it is
        // checked as tainted, whatever order the factory listed them in (design §8.1).
        let builds_risky = batch.iter().any(|c| {
            matches!(c.role, Role::Worker | Role::Fixer)
                && c.unit.as_deref().is_some_and(|u| self.k.is_risky(u))
        });
        if builds_risky && !self.k.tainted && self.k.sandbox_blocker(self.ctx).is_none() {
            self.k.tainted = true;
            self.ctx.journal.append(
                "risk_updated",
                json!({ "tainted": true, "why": "a risky unit in this batch is built in the sandbox" }),
            )?;
        }
        let mut reqs: Vec<(LlmCall, Result<Request, Refusal>)> = Vec::new();
        for c in batch {
            self.ctx.journal.append(
                "command",
                json!({ "label": crate::ids::one_line(&c.label, 64), "what": "llm", "tier": c.tier.name(),
                        "effort": c.effort.map(|e| e.name()), "template": crate::ids::one_line(&c.template, 32) }),
            )?;
            let r = command_check::llm(self.ctx, &self.k, &c, self.max_units);
            if let (Ok(_), Some(unit)) = (&r, c.unit.as_deref()) {
                if super::risk::taints(&mut self.k, c.role, unit) {
                    self.ctx.journal.append(
                        "risk_updated",
                        json!({ "tainted": true, "unit": unit, "why": "a risky unit is being built in the sandbox" }),
                    )?;
                }
            }
            if let Err(Refusal {
                needs_decision: true,
                why,
            }) = &r
            {
                self.ctx
                    .journal
                    .append("warning", json!({ "refused": why }))?;
                return Ok(Some(End::NeedsDecision(self.k.open())));
            }
            reqs.push((c, r));
        }
        let parallel = self
            .ctx
            .shift
            .as_ref()
            .map_or(usize::MAX, |s| s.max_parallel.max(1));
        let mut answers: Vec<Option<Answer>> = Vec::new();
        for chunk in reqs.chunks(parallel) {
            answers.extend(std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|(_, r)| {
                        r.as_ref()
                            .ok()
                            .map(|req| s.spawn(|| runner::call(self.ctx, &self.limited, req)))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.map(|h| h.join().unwrap_or_else(|_| panicked())))
                    .collect::<Vec<_>>()
            }));
        }
        // A Worker's or Reviewer's own sandbox jobs report back: leftovers or a lost job mark the
        // sandbox unusable for the rest of the shift (design §8.5 step 6).
        if let Some(why) = crate::sandbox::client::recorded_fault(&self.ctx.run_dir)? {
            self.ctx.ports.sandbox.mark_broken(&why);
        }
        for ((c, r), a) in reqs.into_iter().zip(answers) {
            let Some(mut a) = a else {
                let why = r.err().map(|e| e.why).unwrap_or_default();
                self.ctx
                    .journal
                    .append("warning", json!({ "label": c.label, "refused": why }))?;
                self.queue.push_back(Event::LlmFailed {
                    label: c.label,
                    why: LlmFailure::Refused,
                });
                continue;
            };
            if let Some(i) = a.incident.take() {
                return Ok(Some(End::Incident(i)));
            }
            if a.capped {
                return Ok(Some(End::Parked("call cap".into())));
            }
            if a.reply == Err(LlmFailure::RateLimited) {
                return Ok(Some(End::Parked(format!(
                    "rate limited: no usable provider left for {}",
                    c.label
                ))));
            }
            if let Err(e) = self.k.integrity(self.ctx, &c.label) {
                return Ok(Some(if e.starts_with("incident") {
                    End::Incident(e)
                } else {
                    End::Failed(e)
                }));
            }
            if let Some(end) = self.after(&c, r.ok(), &mut a)? {
                return Ok(Some(end));
            }
            self.queue.push_back(match a.reply {
                Ok(reply) => Event::LlmDone {
                    label: c.label,
                    reply,
                    cost_micros: a.cost_micros,
                    counts_for_tier: a.counts_for_tier,
                },
                Err(why) => Event::LlmFailed {
                    label: c.label,
                    why,
                },
            });
        }
        Ok(None)
    }

    /// Kernel bookkeeping for one answer, before the factory sees it.
    fn after(
        &mut self,
        c: &LlmCall,
        req: Option<Request>,
        a: &mut Answer,
    ) -> Result<Option<End>, String> {
        match (&c.role, &a.reply) {
            (Role::Reader, Ok(Reply::Reading(r))) if !r.units.is_empty() => {
                let mut r = r.clone();
                self.k.after_read(self.ctx, &r)?;
                let mut blockers = self.ctx.ports.hx.gate_ready_to_build()?;
                if blockers != [PENDING_APPROVAL] {
                    let Some(mut req) = req else { return Ok(None) };
                    req.label = "read!gate".into();
                    req.prompt = format!(
                        "{}\n\nThe ready-to-build gate still has blockers other than the pending approval. Fix the records until only that one remains:\n{}",
                        req.prompt,
                        blockers.iter().map(|b| format!("- {b}")).collect::<Vec<_>>().join("\n")
                    );
                    let again = runner::call(self.ctx, &self.limited, &req);
                    if let Some(i) = again.incident {
                        return Ok(Some(End::Incident(i)));
                    }
                    if again.capped {
                        return Ok(Some(End::Parked("call cap".into())));
                    }
                    if let Err(e) = self.k.integrity(self.ctx, "read!gate") {
                        return Ok(Some(End::Incident(e)));
                    }
                    if let Ok(Reply::Reading(r2)) = &again.reply {
                        r = r2.clone();
                        self.k.after_read(self.ctx, &r)?;
                    }
                    blockers = self.ctx.ports.hx.gate_ready_to_build()?;
                    if blockers != [PENDING_APPROVAL] || again.reply.is_err() {
                        return Ok(Some(End::Failed(format!(
                            "the Reading does not pass ready-to-build: {}",
                            blockers.join(" | ")
                        ))));
                    }
                    a.reply = Ok(Reply::Reading(r.clone()));
                }
                self.ctx.journal.append(
                    "gate_evaluated",
                    json!({ "gate": "ready_to_build", "blockers": blockers }),
                )?;
                // The self lane: a plan that names a protected path ends here, before any edit.
                if let Some(why) = super::publish::self_plan_refusal(self.ctx, &r) {
                    self.ctx
                        .journal
                        .append("warning", json!({ "refused": why }))?;
                    return Ok(Some(End::Failed(why)));
                }
                let view = self.k.risk_view(self.ctx);
                self.ctx.journal.append(
                    "risk_updated",
                    json!({ "risky": view.risky_units, "tainted": view.tainted, "sandbox_ready": view.sandbox_ready }),
                )?;
                if let Some(why) = super::risk::park_before_build(self.ctx, &self.k, &view) {
                    self.ctx
                        .journal
                        .append("parked", json!({ "reason": why }))?;
                    return Ok(Some(End::Parked(why)));
                }
                self.queue.push_back(Event::RiskUpdated(view));
                self.queue
                    .push_back(Event::KernelEscalations(self.k.kernel_only()));
            }
            (Role::Reader, Ok(Reply::Reading(r))) => self.k.reading = Some(r.clone()),
            (Role::Challenger, Ok(Reply::Challenge(ch))) => {
                self.k.challenge_escalations = ch.escalations.clone();
                self.k.challenge_added = ch.added.clone();
            }
            (Role::ReleaseWriter, Ok(reply)) => {
                if let Reply::ReleasePlan(p) = reply {
                    self.k.release_plan_ready = Some(p.ready);
                }
                super::stage_exec::release_reply(self.ctx, reply)?
            }
            (Role::Reviewer, Ok(Reply::Review(rv))) => {
                self.k.review = Some(rv.clone());
                if rv.passed && c.tier == Tier::Opus {
                    let tree = self.ctx.ports.repo.tree()?;
                    self.k.review_pass = Some(ReviewPass::new(&self.k.task_id(), &tree, &c.label));
                }
            }
            _ => {}
        }
        Ok(None)
    }
}

fn panicked() -> Answer {
    Answer {
        reply: Err(LlmFailure::Died),
        cost_micros: None,
        counts_for_tier: false,
        incident: None,
        capped: false,
    }
}
