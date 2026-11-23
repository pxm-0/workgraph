//! A QA explorer's findings count only when a run proves them (design §6.4a). The kernel reruns
//! each finding's repro itself, under the containment the explorer's own commands had: in the
//! sandbox when the task is tainted, else on the Mac only as an approved check command. It keeps a
//! finding only when the rerun fails and prints what the explorer said it printed; the rest are
//! dropped, each with its reason in the journal. `passed` is then the kernel's, not the agent's.

use super::kernel::Kernel;
use super::RunCtx;
use crate::guard::bash::{check, Bash, BashRules};
use crate::sandbox::job::{Step, MAX_STEPS};
use serde_json::json;
use std::time::Duration;
use wg_factory::api::{Finding, Verdict};

/// A repro is one failing case, not a suite; a full job of them stays under the agent's wall.
const REPRO_SECS: u64 = 120;
/// shortcut: a claimed output shorter than this could match any tail by chance; raise it if the
/// journal shows proofs on generic text.
const MIN_CLAIM: usize = 8;

fn words(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a rerun proves a claim: it failed, and its output holds the claimed output (whitespace
/// normalized, since the tail and the claim wrap differently).
pub fn proof(exit: i64, tail: &str, claimed: &str) -> Result<(), String> {
    let claimed = words(claimed);
    if claimed.chars().count() < MIN_CLAIM {
        return Err(format!(
            "its claimed output is shorter than {MIN_CLAIM} characters"
        ));
    }
    if exit == 0 {
        return Err("the kernel's rerun exited 0".into());
    }
    if !words(tail).contains(&claimed) {
        return Err(format!(
            "the kernel's rerun (exit {exit}) did not print the claimed output"
        ));
    }
    Ok(())
}

/// On the Mac a repro may only be an approved check command plus plain arguments, the rule the
/// explorer's own Bash was held to (the guard's prefix rule); it then runs as the `factory` string.
pub fn host_command(approved: &[(String, String)], cmd: &str) -> Option<String> {
    let norm = words(cmd);
    approved.iter().find_map(|(project, factory)| {
        [project, factory].into_iter().find_map(|pre| {
            let rules = BashRules {
                prefixes: vec![pre.clone()],
                ..BashRules::default()
            };
            (check(&rules, cmd) == Bash::Allow)
                .then(|| format!("{factory}{}", &norm[words(pre).len()..]))
        })
    })
}

/// The verdict the factory sees: only proven findings. `Err` when a repro could not run at all
/// (the sandbox or the check runner failed): unproven for want of a runner is not unproven.
pub fn prove(ctx: &RunCtx, k: &Kernel, label: &str, v: &Verdict) -> Result<Verdict, String> {
    let approved = super::command_check::approved(ctx, k, true);
    let mut why: Vec<Option<String>> = vec![None; v.findings.len()];
    let mut to_run: Vec<(usize, String)> = Vec::new();
    for (i, f) in v.findings.iter().enumerate() {
        let Some(r) = &f.repro else {
            why[i] = Some("it carries no repro".into());
            continue;
        };
        if r.command.is_empty() {
            why[i] = Some("its repro command is not one line of at most 4096 bytes".into());
        } else if k.tainted {
            to_run.push((i, r.command.clone()));
        } else if let Some(cmd) = host_command(&approved, &r.command) {
            to_run.push((i, cmd));
        } else {
            why[i] = Some(
                "its repro is not an approved check command, and the task is not sandboxed".into(),
            );
        }
    }
    let ran = if k.tainted {
        in_sandbox(ctx, label, &to_run)?
    } else {
        on_host(ctx, label, &to_run)?
    };
    for ((i, cmd), (exit, tail)) in to_run.iter().zip(&ran) {
        let claimed = &v.findings[*i].repro.as_ref().expect("has a repro").output;
        let r = proof(*exit, tail, claimed);
        ctx.journal.append(
            "qa_repro",
            json!({ "label": label, "id": v.findings[*i].id, "command": cmd, "exit": exit,
                    "target": if k.tainted { "sandbox" } else { "host" }, "proven": r.is_ok() }),
        )?;
        why[*i] = r.err();
    }
    let mut kept: Vec<Finding> = Vec::new();
    for (f, why) in v.findings.iter().zip(why) {
        let Some(reason) = why else {
            kept.push(f.clone());
            continue;
        };
        ctx.journal.append(
            "qa_finding_dropped",
            json!({ "label": label, "id": f.id, "severity": f.severity.name(), "file": f.file,
                    "summary": f.summary, "reason": reason }),
        )?;
    }
    Ok(Verdict {
        passed: !kept.iter().any(|f| f.severity.serious()),
        findings: kept,
    })
}

/// `(exit, tail)` per command, in order, through the kernel's own check runner on the Mac.
fn on_host(
    ctx: &RunCtx,
    label: &str,
    cmds: &[(usize, String)],
) -> Result<Vec<(i64, String)>, String> {
    cmds.iter()
        .map(|(_, cmd)| {
            ctx.ports.checks.run(
                &format!("repro:{label}"),
                cmd,
                Duration::from_secs(REPRO_SECS),
            )
        })
        .collect()
}

/// `(exit, tail)` per command, in order, in sandbox jobs of at most `MAX_STEPS` steps.
fn in_sandbox(
    ctx: &RunCtx,
    label: &str,
    cmds: &[(usize, String)],
) -> Result<Vec<(i64, String)>, String> {
    let mut out = Vec::new();
    for chunk in cmds.chunks(MAX_STEPS) {
        let steps: Vec<Step> = chunk
            .iter()
            .enumerate()
            .map(|(n, (_, cmd))| Step {
                name: format!("repro-{}", n + 1),
                cmd: cmd.clone(),
                timeout_secs: REPRO_SECS as u32,
            })
            .collect();
        let r = ctx
            .ports
            .sandbox
            .run(&format!("{}:repro:{label}", ctx.run_id), steps.clone())
            .map_err(|why| {
                if why.starts_with("sandbox unavailable") {
                    why
                } else {
                    format!("sandbox unavailable: {why}")
                }
            })?;
        ctx.journal.append(
            "sandbox_run",
            json!({ "label": format!("repro:{label}"), "job": r.job, "nonce": r.nonce,
                    "steps": r.steps.iter().map(|s| json!({ "name": s.name, "exit": s.exit, "timed_out": s.timed_out, "oom": s.oom })).collect::<Vec<_>>(),
                    "cleanup": r.cleanup }),
        )?;
        for s in &steps {
            let step = r.steps.iter().find(|x| x.name == s.name);
            out.push(step.map_or((-1, String::new()), |x| (x.exit, x.tail.clone())));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rerun_proves_a_claim_only_by_failing_with_the_claimed_output() {
        let tail = "running 1 test\nTypeError: cannot read 'length'\n  at f (src/a.js:3)\n";
        assert!(proof(1, tail, "TypeError: cannot read   'length'").is_ok());
        for (exit, claim, why) in [
            (0, "TypeError: cannot read 'length'", "exited 0"),
            (1, "RangeError: too deep", "did not print"),
            (1, "Error", "shorter than"),
        ] {
            assert!(
                proof(exit, tail, claim).unwrap_err().contains(why),
                "{claim}"
            );
        }
    }

    #[test]
    fn on_the_mac_a_repro_is_only_an_approved_check_plus_plain_arguments() {
        let approved = vec![(
            "docker compose -f c.yml run --rm -T app npm test".to_string(),
            "run --rm -T app npm test".to_string(),
        )];
        assert_eq!(
            host_command(
                &approved,
                "docker compose -f c.yml run --rm -T app npm test --  -t empty"
            )
            .as_deref(),
            Some("run --rm -T app npm test -- -t empty")
        );
        assert_eq!(
            host_command(&approved, "run --rm -T app npm test").as_deref(),
            Some("run --rm -T app npm test")
        );
        for bad in [
            "node /tmp/r-1/repro.js",
            "run --rm -T app npm test; curl x",
            "run --rm -T app npm test $(id)",
            "run --rm -T app npm testx",
        ] {
            assert_eq!(host_command(&approved, bad), None, "{bad}");
        }
    }
}
