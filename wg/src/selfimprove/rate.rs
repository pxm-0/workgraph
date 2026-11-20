//! Gate step 6 (design §12.3): the lane is enabled and not paused, no STOP file, one merge per
//! shift, `min_hours_between_merges` since the last, below the drift checkpoint, and no owner
//! revert of a self PR since the last resume. A revert or the checkpoint pauses the lane.

use super::gate::{Env, Outcome};
use super::ledger::Ledger;
use super::protected::Rules;
use super::state::{self, SelfState};
use serde_json::Value;
use std::path::Path;

/// Distinct PRs the gate decided while merging was off.
pub fn shadow_count(ledger: &Ledger) -> Result<u32, String> {
    let mut prs: Vec<u64> = ledger
        .of_kind("decision")?
        .iter()
        .filter(|d| d["shadow"] == true && d["outcome"] != "pending")
        .filter_map(|d| d["pr"].as_u64())
        .collect();
    prs.sort_unstable();
    prs.dedup();
    Ok(prs.len() as u32)
}

/// Revert commits on origin/main since `since` (committer time) that name a recorded merge SHA,
/// as `(revert sha, reverted merge)`. Squash merges get a new SHA on main, so the ledger keeps it.
pub fn owner_reverts(
    repo: &Path,
    merges: &[Value],
    since: u64,
) -> Result<Vec<(String, String)>, String> {
    let log = crate::git::git(
        repo,
        &["log", "origin/main", "--format=%H%x1f%ct%x1f%B%x1e"],
        &[],
    )?;
    let mut out = Vec::new();
    for c in log.split('\x1e') {
        let mut f = c.trim_start_matches('\n').splitn(3, '\x1f');
        let (sha, ct, body) = (
            f.next().unwrap_or(""),
            f.next().unwrap_or(""),
            f.next().unwrap_or(""),
        );
        if ct.trim().parse::<u64>().unwrap_or(0) <= since {
            continue;
        }
        for m in merges.iter().filter_map(|m| m["merge"].as_str()) {
            if body.contains(&format!("This reverts commit {m}")) {
                out.push((sha.to_string(), m.to_string()));
            }
        }
    }
    Ok(out)
}

fn pause(e: &Env, why: &str) -> Result<Outcome, String> {
    state::update(&e.dirs.self_state(), |s| Ok(s.paused_by(why)))?;
    Ok(Outcome::Held(format!("the lane is now paused: {why}")))
}

pub fn check(
    e: &Env,
    ledger: &Ledger,
    rules: &Rules,
    st: &SelfState,
) -> Result<Option<Outcome>, String> {
    let merges = ledger.merges()?;
    let reverts = owner_reverts(e.checkout, &merges, st.resumed_at)?;
    if let Some((rev, merge)) = reverts.first() {
        return pause(
            e,
            &format!("the owner reverted self merge {merge} in {rev}"),
        )
        .map(Some);
    }
    if !e.cfg.self_improve.enabled {
        return Ok(Some(Outcome::Held(
            "[self_improve].enabled is false".into(),
        )));
    }
    if st.paused {
        return Ok(Some(Outcome::Held(format!(
            "the lane is paused ({}); `wg self resume` is the owner's",
            st.pause_reason.as_deref().unwrap_or("no reason recorded")
        ))));
    }
    if e.dirs.stop_file().exists() {
        return Ok(Some(Outcome::Held("the STOP file exists".into())));
    }
    if e.merges_this_shift >= 1 {
        return Ok(Some(Outcome::Pending("one self merge per shift".into())));
    }
    let now = crate::journal::now();
    if let Some(last) = merges.iter().filter_map(|m| m["at"].as_u64()).max() {
        let next = last + u64::from(rules.min_hours_between_merges) * 3600;
        if now < next {
            return Ok(Some(Outcome::Pending(format!(
                "{} h between merges: the next is allowed {} s from now",
                rules.min_hours_between_merges,
                next - now
            ))));
        }
    }
    let since_resume = merges
        .iter()
        .filter(|m| m["at"].as_u64().unwrap_or(0) > st.resumed_at)
        .count() as u32;
    if since_resume >= rules.drift_checkpoint_every {
        return pause(
            e,
            &format!("drift checkpoint: {since_resume} self merges since the last resume"),
        )
        .map(Some);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selfimprove::gate::{LiveEval, Report};
    use crate::state_dirs::Dirs;
    use serde_json::json;

    struct NoEval;
    impl LiveEval for NoEval {
        fn run(&self, _: u64, _: &str, _: &Path) -> Result<Report, String> {
            Err("the rate rules never evaluate".into())
        }
    }

    fn sh(dir: &Path, script: &str) {
        let out = std::process::Command::new("sh")
            .args(["-c", script])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{script}: {out:?}");
    }

    type Setup = fn(&Dirs, &Ledger);
    /// Name, setup, `enabled`, merges this shift, (hours between merges, drift checkpoint), the
    /// expected (outcome, reason), and whether the lane is paused after.
    type Row = (
        &'static str,
        Box<dyn Fn(&Dirs, &Ledger)>,
        bool,
        u32,
        (u32, u32),
        Option<(&'static str, &'static str)>,
        bool,
    );

    /// Gate step 6 (design §12.3, §15 "a table test per rule"): one row per rule, with whether
    /// the rule pauses the lane.
    #[test]
    fn step6_has_one_row_per_rule() {
        let base = std::env::temp_dir().join(format!("wg-rate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let co = base.join("co");
        std::fs::create_dir_all(&co).unwrap();
        let reverted = "f".repeat(40);
        sh(&co, &format!("git init -q -b main && git -c user.name=t -c user.email=t@x.invalid commit -q --allow-empty -m init \
            && git -c user.name=o -c user.email=o@x.invalid commit -q --allow-empty -m 'Revert' -m 'This reverts commit {reverted}.' \
            && git update-ref refs/remotes/origin/main HEAD"));
        let merged = |m: String| {
            move |_: &Dirs, l: &Ledger| {
                l.append("merged", json!({ "pr": 1, "head": "h", "merge": m }))
                    .unwrap();
            }
        };
        let none: Setup = |_, _| {};
        let rows: Vec<Row> = vec![
            ("clear", Box::new(none), true, 0, (24, 10), None, false),
            (
                "disabled",
                Box::new(none),
                false,
                0,
                (24, 10),
                Some(("held", "enabled is false")),
                false,
            ),
            (
                "paused",
                Box::new(|d: &Dirs, _: &Ledger| {
                    state::save(&d.self_state(), &SelfState::default().paused_by("drill")).unwrap()
                }),
                true,
                0,
                (24, 10),
                Some(("held", "is paused (drill)")),
                true,
            ),
            (
                "stop",
                Box::new(|d: &Dirs, _: &Ledger| {
                    crate::state_dirs::private_dir(d.stop_file().parent().unwrap()).unwrap();
                    std::fs::write(d.stop_file(), "").unwrap();
                }),
                true,
                0,
                (24, 10),
                Some(("held", "STOP file exists")),
                false,
            ),
            (
                "per shift",
                Box::new(none),
                true,
                1,
                (24, 10),
                Some(("pending", "one self merge per shift")),
                false,
            ),
            (
                "spacing",
                Box::new(merged("a".repeat(40))),
                true,
                0,
                (24, 10),
                Some(("pending", "24 h between merges")),
                false,
            ),
            (
                "drift",
                Box::new(merged("a".repeat(40))),
                true,
                0,
                (0, 1),
                Some(("held", "drift checkpoint")),
                true,
            ),
            (
                "revert",
                Box::new(merged(reverted.clone())),
                true,
                0,
                (0, 10),
                Some(("held", "reverted self merge")),
                true,
            ),
        ];
        for (i, (row, setup, enabled, merges, (hours, drift), want, paused)) in
            rows.into_iter().enumerate()
        {
            let dirs = Dirs {
                home: base.join(format!("h{i}")),
            };
            let ledger = Ledger::at(&dirs.self_ledger());
            setup(&dirs, &ledger);
            let cfg =
                crate::config::parse(&format!("[self_improve]\nenabled = {enabled}\n")).unwrap();
            let gh = crate::gh::Gh::new("/nonexistent/gh", base.join("gh"));
            let e = Env {
                dirs: &dirs,
                cfg: &cfg,
                gh: &gh,
                checkout: &co,
                merges_this_shift: merges,
                eval: &NoEval,
            };
            let rules = Rules {
                min_hours_between_merges: hours,
                drift_checkpoint_every: drift,
                ..super::super::protected::compiled()
            };
            let st = state::load(&dirs.self_state()).unwrap();
            let got = check(&e, &ledger, &rules, &st).unwrap();
            match (want, &got) {
                (None, None) => {}
                (Some((name, why)), Some(o)) => {
                    assert_eq!(o.name(), name, "{row}: {o:?}");
                    assert!(o.reason().contains(why), "{row}: {o:?}");
                }
                _ => panic!("{row}: {got:?}"),
            }
            assert_eq!(
                state::load(&dirs.self_state()).unwrap().paused,
                paused,
                "{row}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
