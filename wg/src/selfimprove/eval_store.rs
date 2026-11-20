//! The real Tier B ports. `StoreEval` checks the pinned fixture store and runs `evaluate` with
//! `ProcArms`, which runs each arm as `wg eval arm` (this same base binary, a fresh process per arm).
//! `arm` is that subcommand: a throwaway clone of the fixture's bundle at its base commit, the dev
//! chain with base assets or the candidate's overlay, every check in the sandbox (the run starts
//! tainted), a throwaway memory, then the hidden tests copied in and run in the sandbox.

use super::eval_live::{self, Arm, ArmResult, Arms, Fixture};
use super::gate::{LiveEval, Report};
use super::replay_check;
use crate::config::Config;
use crate::engine::driver::{self, Start};
use crate::engine::RunCtx;
use crate::git::git;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn store(dirs: &Dirs) -> PathBuf {
    dirs.state().join("eval").join("store")
}

/// The store is at the pinned commit with the pinned tree, and holds enough fixtures.
pub fn checked_store(dirs: &Dirs) -> Result<Vec<Fixture>, String> {
    checked_store_at(
        &store(dirs),
        &replay_check::manifest(replay_check::COMPILED)?,
    )
}

pub fn checked_store_at(s: &Path, m: &replay_check::Manifest) -> Result<Vec<Fixture>, String> {
    if m.store.commit.is_empty() {
        return Err("no fixture store is pinned in eval/manifest.toml yet".into());
    }
    let head = git(s, &["rev-parse", "HEAD"], &[])
        .map_err(|e| format!("the fixture store at {} is missing: {e}", s.display()))?;
    if head != m.store.commit {
        return Err(format!(
            "the fixture store is at {head}, the pin is {}",
            m.store.commit
        ));
    }
    let tree = git(s, &["ls-tree", "-r", &head], &[])?;
    if format!("{:x}", Sha256::digest(tree.as_bytes())) != m.store.tree_sha256 {
        return Err("the fixture store's tree does not match its pinned sha256".into());
    }
    let f = eval_live::fixtures(s)?;
    eval_live::enough(&f, m.store.min_fixtures, m.store.min_needs_decision)?;
    Ok(f)
}

pub struct StoreEval<'a> {
    pub dirs: &'a Dirs,
    pub exe: PathBuf,
}

impl LiveEval for StoreEval<'_> {
    fn run(&self, pr: u64, head: &str, overlay: &Path) -> Result<Report, String> {
        let fixtures = checked_store(self.dirs)?;
        let arms = ProcArms {
            exe: self.exe.clone(),
            overlay: overlay.to_path_buf(),
            tag: format!("eval-{pr}"),
        };
        let seed = pr ^ u64::from_str_radix(head.get(..12).unwrap_or("0"), 16).unwrap_or(0);
        let o = eval_live::evaluate(&fixtures, &arms, seed)?;
        Ok(Report {
            passed: o.passed,
            reason: o.reason,
            spend: o.spend,
        })
    }
}

struct ProcArms {
    exe: PathBuf,
    overlay: PathBuf,
    tag: String,
}

impl Arms for ProcArms {
    fn run(&self, fixture: &Fixture, arm: Arm) -> Result<ArmResult, String> {
        let mut c = std::process::Command::new(&self.exe);
        c.args(["eval", "arm", "--fixture"]).arg(&fixture.dir);
        c.args(["--arm", arm.name(), "--tag", &self.tag]);
        if arm == Arm::Candidate {
            c.arg("--overlay").arg(&self.overlay);
        }
        let out = c
            .output()
            .map_err(|e| format!("wg eval arm failed to start: {e}"))?;
        let line = String::from_utf8_lossy(&out.stdout);
        let v: Value = serde_json::from_str(line.trim())
            .map_err(|_| "wg eval arm printed no JSON line".to_string())?;
        if let Some(e) = v["error"].as_str() {
            return Err(format!("{} arm of {}: {e}", arm.name(), fixture.name));
        }
        Ok(ArmResult {
            status: v["status"].as_str().unwrap_or("").into(),
            escalations: strings(&v["escalations"]),
            gates: strings(&v["gates"]),
            schema_ok: v["schema_ok"] == true,
            passed: v["passed"] == true,
            cost_micros: v["cost_micros"].as_u64().unwrap_or(0),
            incidents: v["incidents"].as_u64().unwrap_or(0) as u32,
            by_provider: serde_json::from_value(v["by_provider"].clone()).unwrap_or_default(),
        })
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

fn one(dir: &Path) -> Result<Fixture, String> {
    let parent = dir.parent().ok_or("a fixture dir has a parent")?;
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    eval_live::fixtures(parent.parent().ok_or("fixtures/<name>")?)?
        .into_iter()
        .find(|f| f.name == name)
        .ok_or_else(|| format!("no fixture at {}", dir.display()))
}

/// `wg eval arm`: one arm of one fixture, as one JSON line.
pub fn arm(
    dirs: &Dirs,
    cfg: &Config,
    dir: &Path,
    arm: &str,
    overlay: Option<&Path>,
    tag: &str,
) -> Result<Value, String> {
    let fx = one(dir)?;
    let repo_cfg = cfg
        .repo(&fx.repo)
        .ok_or_else(|| format!("{} is not a [[repo]]", fx.repo))?;
    let mut sb = repo_cfg
        .sandbox
        .clone()
        .ok_or_else(|| format!("{} has no [repo.sandbox]", fx.repo))?;
    sb.image = fx.image.clone();
    let run_id = format!("r-{}", crate::tokens_store::random_hex(6)?);
    let work = dirs
        .eval_work()
        .join(tag)
        .join(format!("{}-{arm}-{run_id}", fx.name));
    crate::state_dirs::private_dir(&work)?;
    let repo = work.join("repo");
    git(
        &work,
        &[
            "clone",
            "-q",
            &dir.join("repo.bundle").to_string_lossy(),
            "repo",
        ],
        &[],
    )?;
    git(
        &repo,
        &[
            "checkout",
            "-q",
            "-b",
            &format!("eval/{arm}"),
            &fx.base_commit,
        ],
        &[],
    )?;
    let repo = repo.canonicalize().map_err(|e| e.to_string())?;
    let assets = match overlay {
        Some(o) => crate::assets::Assets::compiled().overlay(o)?,
        None => crate::assets::Assets::compiled(),
    };
    let chain = crate::pipelines::chain(
        &assets,
        &["dev".to_string()],
        &Default::default(),
        cfg.max_calls_per_task,
    )?;
    let pin = cfg.haruspex.clone().ok_or("no Haruspex CLI is pinned")?;
    let hx = crate::haruspex::cli::PinnedHaruspex::new(&pin, &repo, &[dirs.work(), dirs.state()])?;
    let sandbox = crate::sandbox::client::SimSandbox {
        cfg: cfg.clone(),
        repo: Some(sb),
        root: repo.clone(),
        status: Default::default(),
    };
    let run_dir = dirs.run(&run_id);
    for d in [
        run_dir.join("logs"),
        run_dir.join("guard"),
        dirs.run_tmp(&run_id),
    ] {
        crate::state_dirs::private_dir(&d)?;
    }
    let ctx = RunCtx {
        ports: crate::ports::Ports {
            proc: &crate::ports::RealProc,
            checks: &crate::ports::ShellChecks {
                root: repo.clone(),
                prefix: None,
            },
            repo: &crate::ports::GitRepo { root: repo.clone() },
            hx: &hx,
            sandbox: &sandbox,
        },
        cfg,
        dirs: dirs.clone(),
        run_id: run_id.clone(),
        run_dir: run_dir.clone(),
        run_tmp: dirs.run_tmp(&run_id),
        repo: repo.clone(),
        repo_name: fx.repo.clone(),
        journal: crate::journal::Journal::open(&run_dir.join("journal.jsonl"), &run_id)?,
        ledger: crate::budget::Ledger::new(&dirs.ledger(), &run_id, cfg.max_calls_per_task),
        wg_exe: std::env::current_exe().map_err(|e| e.to_string())?,
        haruspex_cli: hx.cli.to_string_lossy().into_owned(),
        plugins: crate::claude::auth::plugin_profile(&dirs.home, &cfg.keep_plugins),
        user: "eval".into(),
        replay: crate::claude::replay::Replay::none(),
        assets: assets.clone(),
        shift: None,
    };
    ctx.journal.append(
        "run_started",
        json!({ "eval": tag, "fixture": fx.name, "arm": arm,
        "binary": crate::claude::replay::binary_hash(), "assets": assets.hash() }),
    )?;
    let task = std::fs::read_to_string(dir.join("task.md"))
        .map_err(|e| format!("the fixture has no task.md: {e}"))?;
    // A throwaway memory: an eval never learns into the owner's memory.
    let memory_path = work.join("memory.json");
    let start = Start {
        title: task.lines().next().unwrap_or_default().to_string(),
        task: crate::untrusted::issue::fenced_title(0, &task),
        decisions: vec![],
        resume_task: None,
        initialized_earlier: false,
        user_changes_before: vec![],
        memory: crate::memory_store::load(&memory_path),
        memory_path,
        chain,
        start_branch: format!("eval/{arm}"),
        initializing: !repo.join(".haruspex").exists(),
        baseline_floor: None,
        tainted_earlier: true,
    };
    let ran = driver::run_task(&ctx, start, &wg_factory::engine::step)?;
    let out = crate::report::output(&ctx, &ran)?;
    ctx.journal
        .append("run_finished", json!({ "status": out["status"] }))?;
    let hidden_ok = hidden(&ctx, &fx, dir, &repo)?;
    let journal = crate::journal::read(&run_dir.join("journal.jsonl"))?;
    let ledger = crate::journal::read(&dirs.ledger()).unwrap_or_default();
    let r = eval_live::arm_result(
        &out,
        &journal,
        &ledger,
        &run_id,
        &fx.expected_status,
        hidden_ok,
    );
    Ok(
        json!({ "run": run_id, "status": r.status, "escalations": r.escalations, "gates": r.gates,
               "schema_ok": r.schema_ok, "passed": r.passed, "cost_micros": r.cost_micros,
               "incidents": r.incidents, "by_provider": r.by_provider }),
    )
}

/// Copies `hidden/` in after the run (never readable before) and runs the fixture's checks in the
/// sandbox; true when every one passes. A fixture expecting no build has nothing to run.
fn hidden(ctx: &RunCtx, fx: &Fixture, dir: &Path, repo: &Path) -> Result<bool, String> {
    let src = dir.join("hidden");
    if fx.expected_status != "done" || !src.exists() {
        return Ok(true);
    }
    let mut pending = vec![PathBuf::new()];
    while let Some(rel) = pending.pop() {
        for e in std::fs::read_dir(src.join(&rel)).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let r = rel.join(e.file_name());
            if e.file_type().map_err(|e| e.to_string())?.is_dir() {
                crate::state_dirs::private_dir(&repo.join(&r))?;
                pending.push(r);
            } else {
                std::fs::copy(e.path(), repo.join(&r)).map_err(|e| e.to_string())?;
            }
        }
    }
    git(repo, &["add", "-A"], &[])?;
    git(
        repo,
        &[
            "-c",
            "user.name=wg",
            "-c",
            "user.email=wg@localhost",
            "commit",
            "-qm",
            "hidden tests",
            "--allow-empty",
        ],
        &[],
    )?;
    let steps = fx
        .checks
        .iter()
        .map(|(n, c)| crate::sandbox::job::Step {
            name: n.clone(),
            cmd: c.clone(),
            timeout_secs: 900,
        })
        .collect();
    let nonce = crate::tokens_store::random_hex(8)?;
    let r = ctx.ports.sandbox.run(&nonce, steps)?;
    Ok(r.error.is_none() && r.steps.iter().all(|s| s.exit == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sh(dir: &Path, script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn the_store_must_match_its_pin_and_hold_enough_fixtures() {
        let d = std::env::temp_dir().join(format!("wg-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let img = format!("docker.io/library/python@sha256:{}", "c".repeat(64));
        for i in 0..10 {
            let f = d.join(format!("fixtures/f{i}"));
            std::fs::create_dir_all(&f).unwrap();
            let status = if i < 3 { "needs_decision" } else { "done" };
            std::fs::write(f.join("fixture.toml"), format!("repo = \"o/r\"\nbase_commit = \"{}\"\nexpected_status = \"{status}\"\nimage = \"{img}\"\nchecks = {{}}\n", "a".repeat(40))).unwrap();
        }
        sh(&d, "git init -q -b main && git config user.name t && git config user.email t@x.invalid && git add -A && git commit -qm f");
        let head = sh(&d, "git rev-parse HEAD");
        let tree = sh(&d, "git ls-tree -r HEAD");
        let mut m = replay_check::manifest(replay_check::COMPILED).unwrap();
        assert!(
            checked_store_at(&d, &m)
                .unwrap_err()
                .contains("no fixture store is pinned"),
            "the shipped pin is empty"
        );
        m.store.commit = head.clone();
        m.store.tree_sha256 = format!("{:x}", Sha256::digest(tree.as_bytes()));
        assert_eq!(checked_store_at(&d, &m).unwrap().len(), 10);
        assert!(checked_store_at(&d.join("missing"), &m)
            .unwrap_err()
            .contains("missing"));
        let bad_tree = replay_check::Manifest {
            store: replay_check::Store {
                tree_sha256: "0".repeat(64),
                ..m.store.clone()
            },
            ..m.clone()
        };
        assert!(checked_store_at(&d, &bad_tree)
            .unwrap_err()
            .contains("tree"));
        let more = replay_check::Manifest {
            store: replay_check::Store {
                min_fixtures: 11,
                ..m.store.clone()
            },
            ..m.clone()
        };
        assert!(checked_store_at(&d, &more)
            .unwrap_err()
            .contains("at least 11"));
        sh(&d, "git commit -q --allow-empty -m later");
        assert!(checked_store_at(&d, &m).unwrap_err().contains("the pin is"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn each_arm_is_a_fresh_process_and_its_line_is_parsed() {
        let d = std::env::temp_dir().join(format!("wg-procarms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let exe = d.join("wg");
        std::fs::write(&exe, "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/argv\"\ncase \"$*\" in *candidate*) echo '{\"error\":\"no sandbox\"}' ;; *) echo '{\"status\":\"needs_decision\",\"escalations\":[\"E1\"],\"gates\":[\"ready_to_build\"],\"schema_ok\":true,\"passed\":true,\"cost_micros\":7,\"incidents\":0,\"by_provider\":{\"subscription\":7}}' ;; esac\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let arms = ProcArms {
            exe: exe.clone(),
            overlay: d.join("overlay"),
            tag: "eval-7".into(),
        };
        let fx = Fixture {
            name: "f1".into(),
            dir: d.join("f1"),
            repo: "o/r".into(),
            base_commit: "a".repeat(40),
            expected_status: "needs_decision".into(),
            image: String::new(),
            checks: Default::default(),
        };
        let r = arms.run(&fx, Arm::Base).unwrap();
        assert_eq!(
            (r.status.as_str(), r.escalations.clone(), r.cost_micros),
            ("needs_decision", vec!["E1".to_string()], 7)
        );
        assert_eq!(r.by_provider["subscription"], 7);
        let argv = std::fs::read_to_string(d.join("argv")).unwrap();
        assert!(
            argv.contains("eval arm --fixture")
                && argv.contains("--arm base --tag eval-7")
                && !argv.contains("--overlay"),
            "{argv}"
        );
        assert!(arms
            .run(&fx, Arm::Candidate)
            .unwrap_err()
            .contains("no sandbox"));
        assert!(std::fs::read_to_string(d.join("argv"))
            .unwrap()
            .contains("--overlay"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
