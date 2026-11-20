//! Publish (design §4.1, §7.3): the kernel checks every invariant on its own tokens, then commits
//! the worktree, pushes the `wg/*` task branch and opens a draft PR. Idempotent: a PR that already
//! exists for the branch is returned, so a resume never opens a second one. Only planned, non-risky
//! files are ever pushed, and the PR is always a draft: making it ready is `wg approve`'s (§9.5).

use super::kernel::Kernel;
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
    if !k.qa_verified_at(&tree) {
        return Ok(Some(
            "a QA check has no passing result at the current tree".into(),
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

/// Why the changed set may not be pushed: a risky path outside a sandboxed risky unit (design §8.2),
/// or a file no unit planned.
pub fn scope_refusal(k: &Kernel, changed: &[String], initialized: bool) -> Option<String> {
    // A risky unit's own files may ship once the task is tainted: every check then ran in the
    // sandbox and the tokens say so (design §8.1).
    let sandboxed = |f: &str| {
        k.tainted
            && k.risky.iter().any(|(id, _)| {
                k.unit_files(id)
                    .is_some_and(|files| crate::sandbox::patch::in_unit(f, &files))
            })
    };
    let outside_harness: Vec<String> = changed
        .iter()
        .filter(|f| !f.starts_with(".haruspex/") && !sandboxed(f))
        .cloned()
        .collect();
    if let Some(why) = crate::risk::paths(&outside_harness) {
        return Some(format!("the change touches {why}"));
    }
    let out = out_of_scope(changed, &k.units(), &[], &[], initialized);
    (!out.is_empty()).then(|| format!("files no unit planned: {}", out.join(", ")))
}

/// Haruspex's harness files (its records, and the AGENTS.md and CLAUDE.md its init wrote), as
/// `plan::scope` defines them. In the self lane they stay local: they are protected and never ship.
fn harness(f: &str, initialized: bool) -> bool {
    f.starts_with(".haruspex/") || (initialized && (f == "AGENTS.md" || f == "CLAUDE.md"))
}

/// Why a self-lane change may not be pushed: any path off the editable allowlist (Haruspex's own
/// records excepted, which never ship), checked again here whatever the plan said (design §12.1).
pub fn self_refusal(
    repo: &std::path::Path,
    base: &str,
    changed: &[String],
    initialized: bool,
) -> Option<String> {
    let rules = crate::selfimprove::protected::compiled();
    let bad: Vec<String> = changed
        .iter()
        .filter(|f| !harness(f, initialized))
        .filter_map(|f| {
            let existed = crate::git::exists_at(repo, base, f);
            rules.refusal(if existed { 'M' } else { 'A' }, f)
        })
        .collect();
    (!bad.is_empty()).then(|| {
        format!(
            "the self change touches protected paths: {}",
            bad.join("; ")
        )
    })
}

/// In the self lane, why the Reading's plan may not be built: a unit names a protected path. The
/// Workers' write roots are their units' files, so no protected change can even be produced.
pub fn self_plan_refusal(ctx: &RunCtx, r: &wg_factory::api::Reading) -> Option<String> {
    if !ctx
        .shift
        .as_ref()
        .is_some_and(|s| crate::git::self_branch(&s.branch))
    {
        return None;
    }
    let rules = crate::selfimprove::protected::compiled();
    let bad: Vec<String> = r
        .units
        .iter()
        .flat_map(|u| u.files.iter())
        .filter_map(|f| {
            let existed = ctx.repo.join(f).exists();
            rules.refusal(if existed { 'M' } else { 'A' }, f.trim_end_matches('/'))
        })
        .collect();
    (!bad.is_empty()).then(|| format!("the self plan names protected paths: {}", bad.join("; ")))
}

/// The `risky:<category>` labels of the risky units in this task.
pub fn risky_labels(k: &Kernel) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, why) in k.risky.iter().filter(|(id, _)| k.built_risky.contains(id)) {
        let l = format!("risky:{}", crate::risk::category(why));
        if !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// Risky units and the `SandboxRun` evidence for the PR body.
fn sandbox_section(ctx: &RunCtx, k: &Kernel) -> String {
    if !k.tainted {
        return String::new();
    }
    let units: Vec<String> = k
        .risky
        .iter()
        .filter(|(id, _)| k.built_risky.contains(id))
        .map(|(id, why)| format!("- {id}: {}", crate::ids::one_line(why, 200)))
        .collect();
    let runs: Vec<String> = k
        .sandbox_runs
        .iter()
        .map(|r| {
            let steps: Vec<String> = r
                .steps()
                .iter()
                .map(|(n, e)| format!("{n} exit {e}"))
                .collect();
            format!(
                "- {}: job {} nonce {} at tree {}, image {}: {}; cleanup {}",
                r.label(),
                r.job(),
                r.nonce(),
                &r.tree()[..r.tree().len().min(12)],
                r.image(),
                steps.join(", "),
                if r.clean() { "clean" } else { "LEFTOVERS" }
            )
        })
        .collect();
    format!(
        "\nTainted: every check after the risky build ran in the sandbox on `{}` (rootless podman, no network, no host mounts), never on the Mac. Haruspex verify for this task runs there too.\n\nRisky units:\n{}\n\nSandbox evidence (SandboxRun):\n{}\n",
        ctx.cfg.sandbox.machine,
        if units.is_empty() { "- none built (the repo is always tainted)".to_string() } else { units.join("\n") },
        if runs.is_empty() { "- none".to_string() } else { runs.join("\n") },
    )
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
        "Closes #{issue}\n\nDraft PR by a workgraph shift. Nothing here is approved: the owner approves with `/workgraph:work approve {}#<this PR>`, which records Haruspex verification.\n\n- Haruspex task: {}\n- Gate checks, passing at the current tree: {}\n- Checks already failing before the change: {failing}\n- Independent review (opus): passed\n- Run: {}\n\nCriteria verdicts:\n{review}\n{}",
        ctx.repo_name,
        k.task_id(),
        k.gate.join(", "),
        ctx.run_id,
        sandbox_section(ctx, k),
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
    let self_lane = crate::git::self_branch(&on);
    if on != sh.branch || !(crate::git::task_branch(&on) || self_lane) {
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
    if !def.draft && !self_lane {
        return Err("only a self-lane PR opens ready; a shift PR is always a draft".into());
    }
    if self_lane {
        if let Some(why) = self_refusal(&ctx.repo, &base, &changed, initialized) {
            return Err(why);
        }
    }
    if let Some(pr) = sh.gh.pr_for_head(&ctx.repo_name, &sh.branch)? {
        return Ok(pr.url);
    }
    // Harness files stay local in the self lane: they are protected, so they never ship.
    let files: Vec<String> = crate::git::dirty_in(&ctx.repo)?
        .into_iter()
        .filter(|f| !(self_lane && harness(f, initialized)))
        .collect();
    let message = format!(
        "{title}\n\nWorkgraph-Run: {}\nWorkgraph-Shift: {}\n\n{}",
        ctx.run_id, sh.shift_id, sh.attribution
    );
    crate::git::commit(&ctx.repo, &files, &message)?;
    crate::git::push_task_branch(&ctx.repo, &sh.branch)?;
    let mut labels = def.labels.clone();
    for l in risky_labels(k) {
        sh.gh.ensure_label(&ctx.repo_name, &l)?;
        labels.push(l);
    }
    let body = if self_lane {
        format!(
            "Closes #{}\n\nSelf PR by a workgraph shift (run {}). The self-merge gate decides it from the installed base binary after CI: protected paths, lint, check-runs and the eval. It never merges while the lane is in shadow mode.",
            sh.issue, ctx.run_id
        )
    } else {
        body(ctx, k, sh.issue)
    };
    sh.gh.pr_create(
        &ctx.repo_name,
        &sh.branch,
        &sh.base,
        title,
        &body,
        def.draft,
        &labels,
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
        let mut risky = k.clone();
        if let Some(r) = risky.reading.as_mut() {
            r.units[0].files.push("db/migrations/".into());
        }
        let mig = s(&["calc.py", "db/migrations/001.sql"]);
        assert!(
            scope_refusal(&risky, &mig, false).is_some(),
            "not tainted: refused"
        );
        risky.risky = vec![(
            "a".into(),
            "migration: a database migration (db/migrations/)".into(),
        )];
        risky.built_risky = vec!["a".into()];
        risky.tainted = true;
        assert_eq!(
            scope_refusal(&risky, &mig, false),
            None,
            "a sandboxed risky unit's files ship"
        );
        assert!(
            scope_refusal(&risky, &s(&[".github/x.yml"]), false).is_some(),
            "other risky paths do not"
        );
        assert_eq!(super::risky_labels(&risky), ["risky:migration"]);
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
