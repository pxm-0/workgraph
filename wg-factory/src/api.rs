//! The Command/Event contract between the kernel (`wg`) and the factory. Protected: the kernel
//! parses every untrusted input into these types before the factory sees it, and validates every
//! command the factory returns before it reaches an effect (design §4.2).

use alloc::string::String;
use alloc::vec::Vec;

/// Model tiers, cheapest first. The factory names tiers, never providers or model ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Haiku,
    Sonnet,
    Opus,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Haiku, Tier::Sonnet, Tier::Opus];

    pub fn name(self) -> &'static str {
        match self {
            Tier::Haiku => "haiku",
            Tier::Sonnet => "sonnet",
            Tier::Opus => "opus",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.name() == s)
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn name(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Small,
    Large,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Reader,
    Challenger,
    Worker,
    Fixer,
    Reviewer,
    Learner,
    /// A QA `agent` stage: negative and recovery paths, read-only but for `<run>/tmp` (Phase 4).
    QaExplorer,
    /// A release `agent` stage: notes, and the release plan in the Haruspex task (Phase 4).
    ReleaseWriter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaId {
    Reading,
    Challenge,
    Work,
    Review,
    Learn,
    Verdict,
    ReleaseNotes,
    ReleasePlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    pub fn serious(self) -> bool {
        self >= Severity::High
    }
}

/// Which prompts a learned rule reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Planner,
    Worker,
    All,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Planner => "planner",
            Scope::Worker => "worker",
            Scope::All => "all",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Escalation {
    pub id: String,
    pub severity: Severity,
    pub question: String,
    pub recommendation: String,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Criterion {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnitPlan {
    pub id: String,
    pub goal: String,
    pub files: Vec<String>,
    pub depends_on: Vec<String>,
    pub criteria: Vec<String>,
    pub size: Size,
    pub risky: bool,
    pub risk_reason: Option<String>,
}

/// A check the Reading proposes: its name in `project.json` and its command.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckSpec {
    pub name: String,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub branch: String,
    pub note: Option<String>,
    pub task_id: String,
    pub initialized: bool,
    pub risk: Severity,
    pub checks: Vec<CheckSpec>,
    pub criteria: Vec<Criterion>,
    pub units: Vec<UnitPlan>,
    pub escalations: Vec<Escalation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Added {
    pub id: String,
    pub kind: Option<String>,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assign {
    pub criterion_id: String,
    pub unit_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Challenge {
    pub added: Vec<Added>,
    pub assign: Vec<Assign>,
    pub escalations: Vec<Escalation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Work {
    pub summary: String,
    pub blocked: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CriterionVerdict {
    pub id: String,
    pub passed: bool,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub file: Option<String>,
    pub summary: String,
    /// A QA finding's repro, which the kernel reran before the factory sees it (design §6.5).
    pub repro: Option<Repro>,
}

/// What a QA explorer ran to show a finding: one sandbox command that exits non-zero while the
/// problem is there, what it printed, and what correct code would print.
#[derive(Clone, Debug, PartialEq)]
pub struct Repro {
    pub cmd: String,
    pub output: String,
    pub expected: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    pub passed: bool,
    pub criteria: Vec<CriterionVerdict>,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuleDraft {
    pub sig: String,
    pub text: String,
    pub scope: Scope,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub category: String,
    pub proposal: String,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Learn {
    pub rules: Vec<RuleDraft>,
    pub proposals: Vec<Proposal>,
}

/// A QA explorer's verdict: whether the change holds up, and what it found.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub passed: bool,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseNotes {
    pub summary: String,
    pub changes: Vec<String>,
    pub breaking: bool,
}

/// The plan itself is written into the Haruspex task's `release_plan`; this is the writer's word on it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReleasePlan {
    pub ready: bool,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Reading(Reading),
    Challenge(Challenge),
    Work(Work),
    Review(Review),
    Learn(Learn),
    Verdict(Verdict),
    ReleaseNotes(ReleaseNotes),
    ReleasePlan(ReleasePlan),
}

impl Reply {
    /// The boolean field an `agent` stage's gate reads; `None` when the reply has no such field.
    pub fn flag(&self, field: &str) -> Option<bool> {
        match (self, field) {
            (Reply::Review(r), "passed") => Some(r.passed),
            (Reply::Verdict(v), "passed") => Some(v.passed),
            (Reply::ReleaseNotes(n), "breaking") => Some(n.breaking),
            (Reply::ReleasePlan(p), "ready") => Some(p.ready),
            _ => None,
        }
    }

    /// Findings a fix round can blame (a verdict's), else none.
    pub fn findings(&self) -> &[Finding] {
        match self {
            Reply::Verdict(v) => &v.findings,
            Reply::Review(r) => &r.findings,
            _ => &[],
        }
    }
}

/// A learned rule; `a / (a + b)` is its weight.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub id: String,
    pub text: String,
    pub scope: Scope,
    pub sig: String,
    pub a: f64,
    pub b: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stat {
    pub pass: f64,
    pub fail: f64,
}

/// Memory v1 as typed values. `models` keeps the file's key order.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Memory {
    pub runs: u64,
    pub next_id: u64,
    pub checks: Vec<String>,
    pub rules: Vec<Rule>,
    pub models: Vec<(String, Vec<(Tier, Stat)>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub id: String,
    pub answer: String,
}

/// Limits and prompts of the dev pipeline, already validated and clamped by the kernel.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelineDef {
    pub read: LlmStage,
    pub challenge: LlmStage,
    pub max_units: u32,
    pub max_fix_rounds: u32,
    pub build_max_turns: u32,
    pub build_timeout_secs: u32,
    pub worker_prompt: String,
    pub fix_prompt: String,
    pub review: LlmStage,
    pub review_max_fix_rounds: u32,
    pub learn: LlmStage,
}

/// A repo's assembly line (design §6.2): the dev stages, then every later stage in chain order. The
/// release line has no dev stages. In place the chain is dev only and never publishes.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Chain {
    pub dev: Option<PipelineDef>,
    pub stages: Vec<StageDef>,
}

impl Chain {
    pub fn publish(&self) -> Option<&PublishDef> {
        self.stages.iter().find_map(|s| match &s.kind {
            StageKind::Publish(p) => Some(p),
            _ => None,
        })
    }
}

/// One stage after the dev stages. `label` is its id without hyphens, the prefix of its calls'
/// labels; the kernel keeps it unique and apart from the dev labels.
#[derive(Clone, Debug, PartialEq)]
pub struct StageDef {
    pub id: String,
    pub label: String,
    pub kind: StageKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnFail {
    Stop,
    /// One blamed fix round through the build fix loop, then the stage runs again.
    Fix,
}

/// `field == equals` on the reply.
#[derive(Clone, Debug, PartialEq)]
pub struct Gate {
    pub field: String,
    pub equals: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StageKind {
    /// The gate checks plus a named check set (`names`).
    Checks {
        names: Vec<String>,
        on_fail: OnFail,
        max_fix_rounds: u32,
    },
    Agent {
        role: Role,
        llm: LlmStage,
        schema: SchemaId,
        gate: Option<Gate>,
        on_fail: OnFail,
    },
    /// A re-review: skipped when nothing changed since the last passing review.
    Review {
        llm: LlmStage,
        max_fix_rounds: u32,
    },
    /// Named steps in the sim sandbox; `always` or only when the task is tainted.
    Sandbox {
        steps_from: String,
        always: bool,
    },
    /// A deterministic Haruspex gate whose only blockers may be `expect_only`.
    HaruspexGate {
        gate: String,
        expect_only: Vec<String>,
    },
    /// Terminal: the run waits for the owner.
    Human {
        gate: String,
        ask: String,
    },
    Publish(PublishDef),
}

/// The publish stage: a draft PR with these labels. The kernel checks every invariant first.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishDef {
    pub draft: bool,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LlmStage {
    pub tier: Tier,
    pub effort: Option<Effort>,
    pub prompt: String,
    pub max_turns: u32,
    pub timeout_secs: u32,
}

/// The kernel's own risk view: which units it treats as risky, and whether a sandbox exists.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RiskView {
    pub risky_units: Vec<(String, String)>,
    pub tainted: bool,
    pub sandbox_ready: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunInput {
    pub task: String,
    /// One line naming the task (the issue title in a shift); the factory builds the PR title from it.
    pub title: String,
    pub decisions: Vec<Decision>,
    pub resume_task: Option<String>,
    pub initialized_earlier: bool,
    pub user_changes_before: Vec<String>,
    pub memory: Memory,
    /// False when there was no recall or the memory file is corrupt: the run then never saves.
    pub can_save: bool,
    pub chain: Chain,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LlmCall {
    pub label: String,
    pub role: Role,
    pub unit: Option<String>,
    pub tier: Tier,
    pub effort: Option<Effort>,
    pub template: String,
    pub vars: Vec<(String, String)>,
    pub schema: SchemaId,
    pub max_turns: u32,
    pub timeout_secs: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckOutcome {
    pub name: String,
    pub cmd: String,
    pub exit_code: i64,
    pub output_tail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmFailure {
    Died,
    Timeout,
    Invalid,
    TurnLimit,
    BudgetHit,
    RateLimited,
    Refused,
}

// One value per step, moved once; boxing the large variant would only add noise to every match.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Started(RunInput),
    LlmDone {
        label: String,
        reply: Reply,
        cost_micros: Option<u64>,
        counts_for_tier: bool,
    },
    LlmFailed {
        label: String,
        why: LlmFailure,
    },
    ChecksDone {
        label: String,
        results: Vec<CheckOutcome>,
        changed_files: Vec<String>,
    },
    ChecksUnavailable {
        label: String,
        why: String,
    },
    RiskUpdated(RiskView),
    /// Escalations only the kernel can raise (CHECKS, contract), sent right after the Reading.
    KernelEscalations(Vec<Escalation>),
    /// The kernel opened (or found) the draft PR.
    Published {
        url: String,
    },
    /// The kernel refused to publish: an invariant failed, or the push or PR failed.
    PublishRefused {
        why: String,
    },
    /// A Haruspex gate's blockers, evaluated by the kernel.
    GateDone {
        label: String,
        blockers: Vec<String>,
    },
    /// Sandbox steps ran; one outcome per step.
    SandboxDone {
        label: String,
        results: Vec<CheckOutcome>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NothingToDo,
    Stuck,
    ReadyForReview,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnitNote {
    pub id: String,
    pub goal: String,
    pub why: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Allocation {
    pub id: String,
    pub size: Size,
    pub tier: Tier,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RulesReport {
    pub added: Vec<String>,
    pub revised: Vec<String>,
    pub pruned: Vec<String>,
    pub in_play: Vec<String>,
    pub top: Vec<Rule>,
}

/// What the factory reports. The kernel fills `verified`, `reviewed` and the gate from its own
/// tokens, never from here (design §9.5).
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub status: Status,
    pub note: Option<String>,
    pub review: Option<Review>,
    pub done: Vec<String>,
    pub skipped: Vec<UnitNote>,
    pub stuck: Vec<UnitNote>,
    pub fix_rounds: u32,
    pub review_fix_rounds: u32,
    pub out_of_scope: Vec<String>,
    pub committable: Vec<String>,
    pub models: Vec<Allocation>,
    pub rules: RulesReport,
    pub proposals: Vec<Proposal>,
}

// One value per step, moved once; boxing the large variant would only add noise to every match.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Llm(LlmCall),
    /// Check NAMES, never command strings.
    Checks {
        label: String,
        names: Vec<String>,
    },
    /// Terminal: the run ends `needs_decision`.
    Escalate(Vec<Escalation>),
    /// The kernel validates it as v1 and sanitizes every rule before saving.
    SaveMemory(Memory),
    Log(String),
    /// Commit, push `wg/*` and open a draft PR; the kernel checks every invariant (design §4.1).
    Publish {
        title: String,
    },
    /// Evaluate a Haruspex gate (`--json` only, never `--record`).
    Gate {
        label: String,
        gate: String,
    },
    /// Run a configured step set in the sim sandbox, never on the Mac.
    Sandbox {
        label: String,
        steps_from: String,
    },
    /// Terminal: the run stops for the owner at a human gate. Nothing is approved.
    Human {
        gate: String,
        ask: String,
    },
    /// Terminal.
    Finish(Outcome),
}
