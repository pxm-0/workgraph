//! Publish (design §4.1, §7.3): the kernel checks every invariant on its own tokens, then commits
//! the worktree, pushes the `wg/*` task branch and opens a draft PR. Idempotent: a PR that already
//! exists for the branch is returned, so a resume never opens a second one. Only planned, non-risky
//! files are ever pushed, and the PR is always a draft: making it ready is `wg approve`'s (§9.5).

use super::kernel::{classify, Kernel};
use super::RunCtx;
use crate::haruspex::{contract, tamper};
use wg_factory::api::PublishDef;
use wg_factory::plan::scope::out_of_scope;

const TYPES: [&str; 8] = [
    "feat", "fix", "refactor", "docs", "test", "chore", "perf", "ci",
];

/// `^(feat|fix|…)(\([a-z0-9-]{1,24}\))?: [^\n]{1,60}$`, at most 72 characters (design §4.2).
pub fn title_ok(t: &str) -> bool {
    let Some((head, desc)) = t.split_once(": ") else {
        return false;
    };
    let (ty, scope) = match head.split_once('(') {
        Some((ty, rest)) => match rest.strip_suffix(')') {
            Some(sc) => (ty, Some(sc)),
            None => return false,
        },
        None => (head, None),
    };
    let scope_ok = scope.is_none_or(|s| {
        (1..=24).contains(&s.len())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    });
    TYPES.contains(&ty)
        && scope_ok
        && (1..=60).contains(&desc.chars().count())
        && !t.chars().any(char::is_control)
        && t.chars().count() <= 72
}

/// Why publishing now would break an invariant, checked on the kernel's tokens only.
pub fn refusal(ctx: &RunCtx, k: &Kernel, title: &str) -> Result<Option<String>, String> {
    if !title_ok(title) {
        return Ok(Some("the title is not one conventional-commit line".into()));
    }
    let tree = ctx.ports.repo.tree()?;
    if !k.verified_at(&tree) {
        return Ok(Some(
            "a gate check has no passing result at the current tree".into(),
        ));
    }
    if !k
        .review_pass
        .as_ref()
        .is_some_and(|r| r.tree() == tree && r.task() == k.task_id())
    {
        return Ok(Some(
            "no independent opus review passed at the current tree".into(),
        ));
    }
    let open = k.open();
    if !open.is_empty() {
        let ids: Vec<&str> = open.iter().map(|e| e.id.as_str()).collect();
        return Ok(Some(format!("open escalations: {}", ids.join(", "))));
    }
    if let Some(v) = tamper::violation(&k.approvals, &ctx.ports.hx.approvals()?) {
        return Ok(Some(format!("incident: {v}")));
    }
    if let Some(before) = &k.contract {
        let now = contract::snapshot(&ctx.ports.hx.task(&k.task_id())?);
        if let Some(w) = contract::weakened(before, &now, &k.decided) {
            return Ok(Some(format!("incident: {w}")));
        }
    }
    Ok(None)
}

/// Why the changed set may not be pushed: a risky path (design §8.2; Phase 2 skips risky units, so
/// such a file can only come from a check run or a stray write), or a file no unit planned.
pub fn scope_refusal(k: &Kernel, changed: &[String], initialized: bool) -> Option<String> {
    let outside_harness: Vec<String> = changed
        .iter()
        .filter(|f| !f.starts_with(".haruspex/"))
        .cloned()
        .collect();
    if let Some(why) = classify(&outside_harness) {
        return Some(format!("the change touches {why}"));
    }
    let out = out_of_scope(changed, &k.units(), &[], &[], initialized);
    (!out.is_empty()).then(|| format!("files no unit planned: {}", out.join(", ")))
}

fn body(ctx: &RunCtx, k: &Kernel, issue: u64) -> String {
    let review = match &k.review {
        Some(r) => r
            .criteria
            .iter()
            .map(|c| format!("- {}: {}", c.id, if c.passed { "passed" } else { "failed" }))
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    };
    let failing = if k.left_out.is_empty() {
        "none".to_string()
    } else {
        k.left_out.join(", ")
    };
    format!(
        "Closes #{issue}\n\nDraft PR by a workgraph shift. Nothing here is approved: the owner approves with `/workgraph:work approve {}#<this PR>`, which records Haruspex verification.\n\n- Haruspex task: {}\n- Gate checks, passing at the current tree: {}\n- Checks already failing before the change: {failing}\n- Independent review (opus): passed\n- Run: {}\n\nCriteria verdicts:\n{review}\n",
        ctx.repo_name,
        k.task_id(),
        k.gate.join(", "),
        ctx.run_id,
    )
}

/// Publishes, or says why not. `Err` carries a refusal or a failed git or gh step.
pub fn publish(ctx: &RunCtx, k: &Kernel, def: &PublishDef, title: &str) -> Result<String, String> {
    let sh = ctx
        .shift
        .as_ref()
        .ok_or("publish runs only in a shift; in place the skill commits")?;
    if let Some(why) = refusal(ctx, k, title)? {
        return Err(why);
    }
    let on = crate::git::branch(&ctx.repo)?;
    if on != sh.branch || !crate::git::task_branch(&on) {
        return Err(format!(
            "the worktree is on {on:?}, not the task branch {}",
            sh.branch
        ));
    }
    let base = format!("origin/{}", sh.base);
    let initialized = !crate::git::exists_at(&ctx.repo, &base, ".haruspex");
    let changed = crate::git::changed_since(&ctx.repo, &base)?;
    if let Some(why) = scope_refusal(k, &changed, initialized) {
        return Err(why);
    }
    if let Some(pr) = sh.gh.pr_for_head(&ctx.repo_name, &sh.branch)? {
        return Ok(pr.url);
    }
    let files = crate::git::dirty_in(&ctx.repo)?;
    let message = format!(
        "{title}\n\nWorkgraph-Run: {}\nWorkgraph-Shift: {}\n\n{}",
        ctx.run_id, sh.shift_id, sh.attribution
    );
    crate::git::commit(&ctx.repo, &files, &message)?;
    crate::git::push_task_branch(&ctx.repo, &sh.branch)?;
    sh.gh.pr_create(
        &ctx.repo_name,
        &sh.branch,
        &sh.base,
        title,
        &body(ctx, k, sh.issue),
        true,
        &def.labels,
    )
}

#[cfg(test)]
mod tests {
    use super::{scope_refusal, title_ok};
    use crate::engine::kernel::Kernel;

    #[test]
    fn only_planned_non_risky_files_may_be_pushed() {
        use wg_factory::api::{Reading, Severity, Size, UnitPlan};
        let unit = UnitPlan {
            id: "a".into(),
            goal: "g".into(),
            files: vec!["calc.py".into(), "test_calc.py".into()],
            depends_on: vec![],
            criteria: vec![],
            size: Size::Small,
            risky: false,
            risk_reason: None,
        };
        let k = Kernel {
            reading: Some(Reading {
                branch: "wg/1-x".into(),
                note: None,
                task_id: "T-001".into(),
                initialized: false,
                risk: Severity::Low,
                checks: vec![],
                criteria: vec![],
                units: vec![unit],
                escalations: vec![],
            }),
            ..Kernel::default()
        };
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let ok = s(&["calc.py", "test_calc.py", ".haruspex/tasks/T-001.json"]);
        assert_eq!(scope_refusal(&k, &ok, false), None);
        assert!(
            scope_refusal(&k, &s(&["calc.py", ".github/workflows/x.yml"]), false)
                .unwrap()
                .contains("CI configuration")
        );
        assert!(scope_refusal(&k, &s(&[".env"]), false).is_some());
        assert!(scope_refusal(&k, &s(&["calc.py", "Makefile"]), false)
            .unwrap()
            .contains("files no unit planned: Makefile"));
        assert!(scope_refusal(&k, &s(&["CLAUDE.md"]), false).is_some());
        assert_eq!(scope_refusal(&k, &s(&["CLAUDE.md"]), true), None);
    }

    #[test]
    fn titles_follow_the_conventional_commit_rule() {
        for ok in [
            "feat: add x",
            "fix(auth-flow): handle expiry",
            &format!("docs: {}", "a".repeat(60)),
        ] {
            assert!(title_ok(ok), "{ok}");
        }
        for bad in [
            "add x",
            "feature: x",
            "feat:x",
            "feat(Auth): x",
            "feat(x: y",
            "feat: a\nb",
            &format!("feat: {}", "a".repeat(61)),
            "feat: ",
        ] {
            assert!(!title_ok(bad), "{bad:?}");
        }
    }
}
