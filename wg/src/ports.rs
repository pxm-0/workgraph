//! The kernel's ports to the outside: processes, checks, the repo and Haruspex. The engine driver
//! takes them as trait objects, so the flow tests can answer by label with fakes.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use crate::exec::proc::{Proc, RawOutput, RealProc, Spawn};

/// Runs one approved check command.
pub trait CheckExec: Sync {
    /// `(exit code, 40-line tail)`; `Err` only when the runner itself cannot start.
    fn run(&self, label: &str, cmd: &str, timeout: Duration) -> Result<(i64, String), String>;
}

/// What the kernel asks of the repo.
pub trait Repo: Sync {
    fn changed(&self) -> Result<Vec<String>, String>;
    fn tree(&self) -> Result<String, String>;
    /// The unified diff of `files` against HEAD, untracked files included as additions.
    fn diff(&self, files: &[String]) -> Result<String, String>;
}

/// What the kernel asks of Haruspex: deterministic reads and the ready-to-build gate.
pub trait Haruspex: Sync {
    /// The blockers of `gate ready-to-build . --json`.
    fn gate_ready_to_build(&self) -> Result<Vec<String>, String>;
    fn task(&self, id: &str) -> Result<Value, String>;
    fn approvals(&self) -> Result<Value, String>;
    fn project_commands(&self) -> Result<BTreeMap<String, String>, String>;
    /// A hash of `.haruspex/{bin,templates,managed}`; "absent" when none exists.
    fn managed_hash(&self) -> Result<String, String>;
}

/// Where risky units and a tainted task's checks run (design §8): the sim, or nowhere.
pub trait Sandbox: Sync {
    /// `None` when risky work may run; otherwise why it parks.
    fn unavailable(&self) -> Option<String>;
    /// Runs `steps` on the worktree's tracked files; `Err` when the sandbox itself failed.
    fn run(
        &self,
        nonce: &str,
        steps: Vec<crate::sandbox::job::Step>,
    ) -> Result<crate::sandbox::job::JobResult, String>;
}

pub struct Ports<'a> {
    pub proc: &'a dyn Proc,
    pub checks: &'a dyn CheckExec,
    pub repo: &'a dyn Repo,
    pub hx: &'a dyn Haruspex,
    pub sandbox: &'a dyn Sandbox,
}

/// Checks through `sh -c` in the repo root with the scrubbed environment. In a shift, `prefix` is
/// the repo's `factory_compose`, so the approved `factory` string runs in the factory's own compose
/// project (design §7.1, §9.3).
pub struct ShellChecks {
    pub root: PathBuf,
    pub prefix: Option<String>,
}

impl ShellChecks {
    pub fn command(&self, cmd: &str) -> String {
        match &self.prefix {
            Some(p) => format!("{p} {cmd}"),
            None => cmd.to_string(),
        }
    }
}

impl CheckExec for ShellChecks {
    fn run(&self, _label: &str, cmd: &str, timeout: Duration) -> Result<(i64, String), String> {
        if !Path::new("/bin/sh").exists() {
            return Err("/bin/sh is missing, so no check can run".into());
        }
        Ok(crate::exec::run::run_shell_in(
            &self.command(cmd),
            &self.root,
            &crate::exec::env::scrubbed(),
            timeout,
        ))
    }
}

pub struct GitRepo {
    pub root: PathBuf,
}

impl Repo for GitRepo {
    fn changed(&self) -> Result<Vec<String>, String> {
        crate::git::dirty_in(&self.root)
    }

    fn tree(&self) -> Result<String, String> {
        crate::git::tree_fingerprint(&self.root)
    }

    fn diff(&self, files: &[String]) -> Result<String, String> {
        crate::git::diff_with_new(&self.root, files)
    }
}
