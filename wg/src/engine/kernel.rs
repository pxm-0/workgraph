//! The kernel's own view of a run: its copy of the Reading, the escalations it records itself, the
//! gate it derives from its own baseline, its tokens, and the integrity snapshots (design §4.1,
//! §9.2-§9.4). The factory cannot change any of it.

use super::RunCtx;
use crate::haruspex::contract::{self, Contract};
use crate::haruspex::tamper;
use crate::tokens::{CheckResult, ReviewPass};
use serde_json::Value;
use wg_factory::api::{Escalation, Reading, Severity};
use wg_factory::engine::read::dirty_escalation;
use wg_factory::model::Unit;
use wg_factory::plan::scope::dirty_overlap;

/// The one blocker an honest Reading leaves (haruspex.py:867), pinned by a golden test.
pub const PENDING_APPROVAL: &str = "Human approval for ready_to_build is pending.";

#[derive(Default)]
pub struct Kernel {
    pub reading: Option<Reading>,
    pub challenge_escalations: Vec<Escalation>,
    pub challenge_added: Vec<wg_factory::api::Added>,
    pub read_escalations: Vec<Escalation>,
    pub decided: Vec<String>,
    pub resume_task: Option<String>,
    pub user_changes: Vec<String>,
    pub initializing: bool,
    pub start_branch: String,
    pub risky: Vec<(String, String)>,
    pub required: Vec<String>,
    pub baseline_done: bool,
    pub gate: Vec<String>,
    pub left_out: Vec<String>,
    pub checks: Vec<CheckResult>,
    pub review_pass: Option<ReviewPass>,
    /// The latest review reply, for the PR body.
    pub review: Option<wg_factory::api::Review>,
    pub approvals: Value,
    pub managed: String,
    pub contract: Option<Contract>,
    pub seen_sigs: Vec<String>,
    pub pins: std::collections::BTreeMap<String, String>,
    /// See `driver::Start::baseline_floor`.
    pub baseline_floor: Option<Vec<String>>,
}

fn esc(id: &str, question: String, recommendation: &str) -> Escalation {
    Escalation {
        id: id.into(),
        severity: Severity::High,
        question,
        recommendation: recommendation.into(),
        options: vec![],
    }
}

/// Planned files whose path says they are hard to undo or reach outside the repo (design §8.2;
/// Phase 1 classifies planned files only).
pub fn classify(files: &[String]) -> Option<String> {
    const RISKY: [(&str, &str); 9] = [
        ("migration", "a database migration"),
        (".github/workflows/", "CI configuration"),
        ("deploy", "a deploy"),
        ("auth", "authentication or authorization"),
        ("secret", "secrets"),
        ("payment", "payments"),
        (".env", "environment secrets"),
        ("Dockerfile", "a container build"),
        ("terraform", "infrastructure"),
    ];
    files.iter().find_map(|f| {
        let lower = f.to_lowercase();
        RISKY
            .iter()
            .find(|(p, _)| lower.contains(&p.to_lowercase()))
            .map(|(_, why)| format!("{why} ({f})"))
    })
}

impl Kernel {
    pub fn task_id(&self) -> String {
        self.reading
            .as_ref()
            .map(|r| r.task_id.clone())
            .unwrap_or_default()
    }

    pub fn unit_files(&self, id: &str) -> Option<Vec<String>> {
        self.reading
            .as_ref()?
            .units
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.files.clone())
    }

    /// Criteria ids the validators accept from the challenge and the review.
    pub fn known_criteria(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(r) = &self.reading {
            out.extend(r.criteria.iter().map(|c| c.id.clone()));
            out.extend(r.units.iter().flat_map(|u| u.criteria.clone()));
        }
        out.extend(self.challenge_added.iter().map(|a| a.id.clone()));
        out
    }

    /// Records the Reading: risk view, CHECKS pin, accepted-risk escalations, contract snapshot.
    pub fn after_read(&mut self, ctx: &RunCtx, r: &Reading) -> Result<(), String> {
        self.reading = Some(r.clone());
        self.risky = r
            .units
            .iter()
            .filter(|u| !u.risky)
            .filter_map(|u| classify(&u.files).map(|why| (u.id.clone(), why)))
            .collect();
        let mut escalations = r.escalations.clone();
        let task = ctx.ports.hx.task(&r.task_id)?;
        self.required = task["required_checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect();
        if let Some(e) = self.checks_escalation(ctx, r)? {
            escalations.push(e);
        }
        let snap = contract::snapshot(&task);
        for id in contract::unanswered_acceptances(&snap, &self.decided) {
            escalations.push(esc(
                &id,
                format!("The Reading marked {id} accepted_risk. Only you can accept a high or critical risk: accept it, or say how to mitigate it?"),
                "Mitigate it with a criterion and a test",
            ));
        }
        self.contract = Some(snap);
        if self.managed == "absent" {
            self.managed = ctx.ports.hx.managed_hash()?;
        }
        self.read_escalations = escalations;
        Ok(())
    }

    /// CHECKS (design §9.3): every required check is pinned to the owner-approved command.
    fn checks_escalation(
        &mut self,
        ctx: &RunCtx,
        r: &Reading,
    ) -> Result<Option<Escalation>, String> {
        let project = ctx.ports.hx.project_commands()?;
        let mut asks = Vec::new();
        for name in &self.required {
            let reading_cmd = r
                .checks
                .iter()
                .find(|c| &c.name == name)
                .map(|c| &c.command);
            let project_cmd = project.get(name);
            let approved = ctx.cfg.check(&ctx.repo_name, name);
            let pinned = matches!((reading_cmd, project_cmd, approved),
                (Some(rc), Some(pc), Some(a)) if rc == pc && *pc == a.project);
            if pinned {
                self.pins
                    .insert(name.clone(), project_cmd.cloned().unwrap_or_default());
                continue;
            }
            let cmd = project_cmd.or(reading_cmd).cloned().unwrap_or_default();
            let flag = if cmd.starts_with("docker compose") {
                ""
            } else {
                " (not a docker compose command: it would run on the host)"
            };
            asks.push(format!("`{name}` = `{cmd}`{flag}"));
        }
        if self.required.is_empty() {
            asks.push("the task declares no required_checks".into());
        }
        if asks.is_empty() {
            return Ok(None);
        }
        Ok(Some(esc(
            "CHECKS",
            format!(
                "Allow these check commands for {}? {}. On yes: wg config allow-check --repo {} --name <name> --cmd <command> --factory-cmd <command>.",
                ctx.repo_name,
                asks.join("; "),
                ctx.repo_name
            ),
            "Allow them if each is the repo's own check, run in its dev stack",
        )))
    }

    /// The Reading's units, in the factory's model (for the scope helpers).
    pub fn units(&self) -> Vec<Unit> {
        self.reading
            .iter()
            .flat_map(|r| r.units.iter())
            .map(|u| Unit {
                id: u.id.clone(),
                goal: u.goal.clone(),
                files: u.files.clone(),
                depends_on: vec![],
                criteria: vec![],
                size: u.size,
                risky: u.risky,
                risk_reason: None,
                tier: wg_factory::api::Tier::Sonnet,
                counts_for_tier: false,
            })
            .collect()
    }

    /// Every escalation the kernel knows, in run.js's order: Reading, Challenge, DIRTY, then its own.
    pub fn all_escalations(&self) -> Vec<Escalation> {
        let Some(r) = &self.reading else {
            return vec![];
        };
        let mut out: Vec<Escalation> = self
            .read_escalations
            .iter()
            .filter(|e| r.escalations.iter().any(|x| x.id == e.id))
            .cloned()
            .collect();
        out.extend(self.challenge_escalations.iter().cloned());
        let overlap = dirty_overlap(&self.units(), &self.user_changes);
        if !overlap.is_empty() && !self.decided.iter().any(|d| d == "DIRTY") {
            out.push(dirty_escalation(&overlap));
        }
        out.extend(self.kernel_only());
        let mut seen = Vec::new();
        out.retain(|e| {
            let fresh = !seen.contains(&e.id);
            seen.push(e.id.clone());
            fresh
        });
        out
    }

    /// CHECKS and contract escalations: the ones the factory learns only from the kernel.
    pub fn kernel_only(&self) -> Vec<Escalation> {
        let reading_ids: Vec<&str> = self
            .reading
            .iter()
            .flat_map(|r| r.escalations.iter().map(|e| e.id.as_str()))
            .collect();
        self.read_escalations
            .iter()
            .filter(|e| !reading_ids.contains(&e.id.as_str()))
            .cloned()
            .collect()
    }

    /// Escalations no owner decision answers. While any is open, no Worker or Fixer runs.
    pub fn open(&self) -> Vec<Escalation> {
        self.all_escalations()
            .into_iter()
            .filter(|e| !self.decided.contains(&e.id))
            .collect()
    }

    /// After every LLM call: approvals, managed Haruspex files and the contract (design §9.4).
    pub fn integrity(&mut self, ctx: &RunCtx, after: &str) -> Result<(), String> {
        let approvals = ctx.ports.hx.approvals()?;
        if let Some(v) = tamper::violation(&self.approvals, &approvals) {
            return Err(format!("incident after {after}: {v}"));
        }
        let managed = ctx.ports.hx.managed_hash()?;
        if self.managed != "absent" && managed != self.managed {
            return Err(format!(
                "incident after {after}: .haruspex/bin, templates or managed changed"
            ));
        }
        let Some(before) = self.contract.clone() else {
            return Ok(());
        };
        let now = contract::snapshot(&ctx.ports.hx.task(&self.task_id())?);
        if let Some(w) = contract::weakened(&before, &now, &self.decided) {
            return Err(format!("incident after {after}: {w}"));
        }
        if after == "challenge" || after == "challenge!retry" {
            self.contract = Some(contract::union(&before, &now));
        }
        Ok(())
    }

    /// The gate check set: required checks minus those the kernel saw fail at baseline, and on a
    /// rerun only those that also failed at the task's first clean baseline.
    pub fn record_baseline(&mut self, results: &[CheckResult]) {
        let floor = self.baseline_floor.clone();
        self.left_out = results
            .iter()
            .filter(|r| !r.passed())
            .map(|r| r.name().to_string())
            .filter(|n| floor.as_ref().is_none_or(|f| f.contains(n)))
            .collect();
        self.gate = self
            .required
            .iter()
            .filter(|n| !self.left_out.contains(n))
            .cloned()
            .collect();
        self.baseline_done = true;
    }

    /// The latest results pass for every gate check at `tree`.
    pub fn verified_at(&self, tree: &str) -> bool {
        !self.gate.is_empty()
            && self.gate.iter().all(|n| {
                self.checks
                    .iter()
                    .rev()
                    .find(|c| c.name() == n)
                    .is_some_and(|c| c.passed() && c.tree() == tree)
            })
    }

    pub fn baseline_escalation(&self) -> Escalation {
        esc(
            "BASELINE",
            format!(
                "Every required check already fails before any change ({}), so nothing could verify the work. Fix them first, or remove them from required_checks with a note?",
                self.left_out.join(", ")
            ),
            "Fix the failing checks first, then rerun",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_classifier_flags_risky_paths() {
        let f = |p: &str| classify(&[p.to_string()]);
        assert!(f("db/migrations/001.sql").unwrap().contains("migration"));
        assert!(f(".github/workflows/ci.yml").is_some());
        assert!(f("src/auth/login.ts").is_some());
        assert!(f(".env.production").is_some());
        assert_eq!(f("src/calc.py"), None);
        assert_eq!(
            PENDING_APPROVAL,
            "Human approval for ready_to_build is pending."
        );
    }
}
