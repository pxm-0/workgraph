//! The factory's run state: plain owned values, replaced (never shared) on every step.

use crate::api::{
    Allocation, Challenge, CheckOutcome, Escalation, Finding, LlmFailure, LlmStage, Memory,
    PipelineDef, Proposal, Reading, Reply, Review, RiskView, Size, StageDef, Tier, UnitNote,
};
use alloc::string::String;
use alloc::vec::Vec;

/// A unit of the plan, with the criteria the challenge assigned to it and its allocated tier.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub id: String,
    pub goal: String,
    pub files: Vec<String>,
    pub depends_on: Vec<String>,
    pub criteria: Vec<String>,
    pub size: Size,
    pub risky: bool,
    pub risk_reason: Option<String>,
    pub tier: Tier,
    /// The kernel's verdict that this unit's work call is tier evidence (design §5.3 step 4).
    pub counts_for_tier: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FailedCheck {
    pub cmd: String,
    pub exit_code: i64,
    pub sig: String,
    pub output_tail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FixRecord {
    pub layer: String,
    pub round: u32,
    pub units: Vec<String>,
    pub failed: Vec<FailedCheck>,
    pub fix_summaries: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReviewFixRecord {
    pub round: u32,
    pub units: Vec<String>,
    pub findings: Vec<Finding>,
    pub failed_criteria: Vec<String>,
    pub fix_summaries: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelOutcome {
    pub key: String,
    pub tier: Tier,
    pub pass: bool,
}

/// One answer to an outstanding call: a typed reply, or why it failed.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Reply(Reply, bool),
    Failed(LlmFailure),
}

/// Where the run is waiting. Every phase waits for all its outstanding labels.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Phase {
    #[default]
    Start,
    Reading,
    Challenging,
    Baseline,
    Working {
        layer: usize,
    },
    Checking {
        layer: usize,
        round: u32,
    },
    Fixing {
        layer: usize,
        round: u32,
    },
    Reviewing {
        round: u32,
    },
    ReviewFixing {
        round: u32,
    },
    ReviewChecking {
        round: u32,
    },
    Learning,
    /// A `checks` stage: its check run (round 0) or the recheck after fix round `round`.
    StageChecking {
        round: u32,
    },
    StageFixing {
        round: u32,
    },
    /// An `agent` stage's call: run 1, or run 2 after its fix round.
    Agent {
        run: u32,
    },
    AgentFixing,
    AgentChecking,
    Gating,
    Sandboxing,
    /// Waiting for the kernel's answer to `Publish`.
    Publishing,
    Done,
}

/// The review in progress: dev's `review` stage, or a later re-review stage.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewCfg {
    /// `review#<n>` calls and `check:review<n>` rechecks.
    pub prefix: String,
    /// `fix:<unit>#r<n>` fix calls.
    pub fix_prefix: String,
    pub llm: LlmStage,
    pub max_fix_rounds: u32,
}

/// The current layer's progress through work, checks and fix rounds.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LayerState {
    pub ok: Vec<Unit>,
    pub needs_fix: Vec<String>,
    pub sigs: Vec<String>,
    pub culprits: Vec<Unit>,
    pub failed: Vec<CheckOutcome>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunState {
    pub phase: Phase,
    pub task: String,
    pub title: String,
    /// The outcome held while the kernel publishes.
    pub finished: Option<crate::api::Outcome>,
    pub decisions: Vec<crate::api::Decision>,
    pub resume_task: Option<String>,
    pub initialized_earlier: bool,
    pub user_changes_before: Vec<String>,
    pub memory: Memory,
    pub can_save: bool,
    pub pipeline: Option<PipelineDef>,
    /// The stages after dev, in chain order, and the one running (Phase 4).
    pub stages: Vec<StageDef>,
    pub stage_at: Option<usize>,
    /// A fix changed the tree after the last passing review: a re-review must run.
    pub dirty_since_review: bool,
    pub reviewing: Option<ReviewCfg>,
    /// An agent stage's findings, handed to its fixers and to its second run.
    pub agent_problem: String,
    pub in_play: Vec<String>,
    pub probed: Vec<String>,
    pub reading: Option<Reading>,
    pub challenge: Option<Challenge>,
    pub units: Vec<Unit>,
    pub risk: RiskView,
    pub kernel_escalations: Vec<Escalation>,
    pub committable: Vec<String>,
    pub check_cmds: Vec<(String, String)>,
    pub gate: Vec<String>,
    pub left_out: Vec<String>,
    pub preexisting: Vec<String>,
    pub layers: Vec<Vec<String>>,
    pub layer: LayerState,
    pub pending: Vec<String>,
    pub answers: Vec<(String, Answer)>,
    pub done: Vec<String>,
    pub skipped: Vec<UnitNote>,
    pub stuck: Vec<UnitNote>,
    pub fixes: Vec<FixRecord>,
    pub allocation: Vec<Allocation>,
    pub outcomes: Vec<ModelOutcome>,
    pub fixed_sigs: Vec<String>,
    pub blocked: Vec<String>,
    pub built: Vec<Unit>,
    pub changed: Vec<String>,
    pub stopped: bool,
    pub review: Option<Review>,
    pub review_fixes: Vec<ReviewFixRecord>,
    pub proposals: Vec<Proposal>,
}

impl RunState {
    pub fn unit(&self, id: &str) -> Option<&Unit> {
        self.units.iter().find(|u| u.id == id)
    }

    pub fn pipeline(&self) -> &PipelineDef {
        self.pipeline
            .as_ref()
            .expect("the pipeline is set by Started before any other event")
    }
}

/// Pushes `x` unless it is already present (an ordered set).
pub fn add<T: PartialEq + Clone>(v: &mut Vec<T>, x: &T) {
    if !v.contains(x) {
        v.push(x.clone());
    }
}
