//! `wg finalize --run <runId> --by <name> --message M --evidence-message M2`: the owner's one
//! approval after an in-place run. It refuses unless the run's kernel tokens hold at the current
//! tree (design §9.5); then it runs the finalize chain.

use super::args::{not_an_agent, parse};
use crate::haruspex::cli::PinnedHaruspex;
use crate::haruspex::finalize::{finalize_run, RunFinalize};
use crate::haruspex::{contract, tamper};
use crate::ports::Haruspex;
use crate::state_dirs::Dirs;
use crate::tokens::HumanApproval;
use crate::{git, tokens_store};
use serde_json::{json, Value};

fn refuse(why: impl Into<String>) -> Result<Value, String> {
    Err(format!("finalize refused: {}", why.into()))
}

pub fn run(args: &[String]) -> Result<Value, String> {
    let a = parse(args, &[])?;
    a.only(&["run", "by", "message", "evidence-message"])?;
    not_an_agent("wg finalize")?;
    let (run, by) = (a.need("run")?, a.need("by")?);
    let (message, evidence_message) = (a.need("message")?, a.need("evidence-message")?);
    let dirs = Dirs::from_env()?;
    let cfg = super::run::load_config(&dirs)?;
    let repo = super::run::repo_root()?;
    finalize_at(&dirs, &cfg, &repo, run, by, message, evidence_message)
}

/// The finalize chain for run `run` in `repo` (the repo root, or a shift task's worktree): every
/// token re-checked at the current tree, then `finalize_run` with the owner's approval. Changes the
/// process's directory to `repo`, where the chain's git and Haruspex steps run.
pub fn finalize_at(
    dirs: &Dirs,
    cfg: &crate::config::Config,
    repo: &std::path::Path,
    run: &str,
    by: &str,
    message: &str,
    evidence_message: &str,
) -> Result<Value, String> {
    if by.trim().is_empty() {
        return Err("--by must name the approver".into());
    }
    std::env::set_current_dir(repo).map_err(|e| format!("cannot enter {}: {e}", repo.display()))?;
    let run_dir = super::run::run_dir_for(dirs, run)?;
    let key = tokens_store::key(&dirs.key())?;
    let (meta, checks, review, _) = tokens_store::load(&run_dir.join("tokens.jsonl"), &key)?;
    if meta.repo != repo.to_string_lossy() {
        return refuse(format!("run {run} belongs to {}", meta.repo));
    }
    let pin = cfg.haruspex.clone().ok_or("no Haruspex CLI is pinned")?;
    let hx = PinnedHaruspex::new(&pin, repo, &[dirs.work(), dirs.state()])?;
    let tree = git::tree_fingerprint(repo)?;
    if meta.gate.is_empty() {
        return refuse("the run has no gate checks");
    }
    for name in &meta.gate {
        let latest = checks.iter().rev().find(|c| c.name() == name);
        match latest {
            Some(c) if c.passed() && c.tree() == tree => {}
            Some(c) if c.tree() != tree => {
                return refuse(format!("the tree changed since check {name} ran"))
            }
            _ => return refuse(format!("check {name} has no passing result")),
        }
        if !meta.pins.contains_key(name) {
            return refuse(format!("check {name} was never pinned"));
        }
        if meta.tainted && latest.is_some_and(|c| c.target() != crate::tokens::Target::SimSandbox) {
            return refuse(format!(
                "the task is tainted but check {name} last ran on the host"
            ));
        }
    }
    match &review {
        Some(r) if r.tree() == tree && r.task() == meta.task_id => {}
        Some(_) => return refuse("the tree changed since the independent review passed"),
        None => return refuse("no independent review passed"),
    }
    if !meta.open_escalations.is_empty() {
        return refuse(format!(
            "open escalations: {}",
            meta.open_escalations.join(", ")
        ));
    }
    if let Some(v) = tamper::violation(&meta.approvals, &hx.approvals()?) {
        return refuse(format!("incident: {v}"));
    }
    let now = contract::snapshot(&hx.task(&meta.task_id)?);
    if let Some(w) = contract::weakened(&meta.contract, &now, &meta.decided) {
        return refuse(format!("incident: {w}"));
    }
    // The CHECKS pin (design §9.3, §9.5 step 1) holds before any approval is recorded.
    let project = hx.project_commands()?;
    if let Some((name, _)) = meta
        .pins
        .iter()
        .find(|(name, cmd)| project.get(name.as_str()) != Some(*cmd))
    {
        return refuse(format!("check {name} changed in project.json since Read"));
    }
    let pins = if meta.tainted {
        match sandbox_pins(cfg, repo, &meta) {
            Ok(p) => p,
            Err(why) => return refuse(why),
        }
    } else {
        meta.pins.clone()
    };
    let head = git::head(repo)?;
    let approval = HumanApproval::new(&meta.task_id, &head, by);
    let staged: Vec<String> = git::dirty_in(repo)?
        .into_iter()
        .filter(|p| !meta.exclude.contains(p))
        .collect();
    let mut out = finalize_run(
        &approval,
        &RunFinalize {
            cli: &hx.cli.to_string_lossy(),
            message,
            evidence_message,
            staged,
            pins: &pins,
        },
    );
    out["runId"] = json!(run);
    out["taskId"] = json!(meta.task_id);
    Ok(out)
}

/// A tainted task never verifies on the Mac (design §9.5): every pinned check's `project.json`
/// command becomes the repo's `[repo.sandbox].verify` string, so Haruspex verify runs it in the
/// sandbox. Without one for every check, the kernel refuses.
fn sandbox_pins(
    cfg: &crate::config::Config,
    repo: &std::path::Path,
    meta: &tokens_store::RunMeta,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let host_refused =
        "host verify refused for tainted task; add [repo.sandbox].verify or verify yourself";
    let name = git::repo_name(&git::slug(repo));
    let verify = cfg
        .repo(&name)
        .and_then(|r| r.sandbox.as_ref())
        .map(|s| s.verify.clone())
        .unwrap_or_default();
    crate::ids::task_id(&meta.task_id)?;
    let mut pins = std::collections::BTreeMap::new();
    for check in meta.pins.keys() {
        let cmd = verify.get(check).ok_or(host_refused)?;
        pins.insert(check.clone(), cmd.replace("{task}", &meta.task_id));
    }
    let path = repo.join(".haruspex/project.json");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read project.json: {e}"))?;
    let mut v: Value =
        serde_json::from_str(&text).map_err(|e| format!("project.json is not JSON: {e}"))?;
    for (check, cmd) in &pins {
        v["commands"][check] = json!(cmd);
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&v).expect("serializes") + "\n",
    )
    .map_err(|e| format!("cannot pin project.json to the sandbox: {e}"))?;
    Ok(pins)
}
