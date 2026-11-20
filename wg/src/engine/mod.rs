//! The engine driver: runs the factory's step loop, validates every command, and executes it.

pub mod command_check;
pub mod convert;
pub mod driver;
pub mod kernel;
pub mod publish;

use crate::budget::Ledger;
use crate::config::Config;
use crate::journal::Journal;
use crate::ports::Ports;
use crate::state_dirs::Dirs;
use std::path::PathBuf;

/// Everything one run's effects need. Built by `cli/run.rs`, or by tests with fake ports.
pub struct RunCtx<'a> {
    pub ports: Ports<'a>,
    pub cfg: &'a Config,
    pub dirs: Dirs,
    pub run_id: String,
    pub run_dir: PathBuf,
    pub run_tmp: PathBuf,
    /// Canonical repo root; every child runs here.
    pub repo: PathBuf,
    /// `owner/repo`, the key of `factory.toml` checks.
    pub repo_name: String,
    pub journal: Journal,
    pub ledger: Ledger,
    pub wg_exe: PathBuf,
    pub haruspex_cli: String,
    pub plugins: Vec<(String, bool)>,
    pub user: String,
    /// Answers of an interrupted run to feed again (resume); empty for a fresh run.
    pub replay: crate::claude::replay::Replay,
    /// Set in a shift (Phase 2): what publish and the stop conditions need. `None` in place.
    pub shift: Option<ShiftCtx>,
}

/// The shift side of a run: the issue, its task branch and GitHub, and the stop conditions the
/// driver checks between steps (design §7.3 step 9).
pub struct ShiftCtx {
    pub gh: crate::gh::Gh,
    pub issue: u64,
    pub branch: String,
    pub base: String,
    pub attribution: String,
    pub shift_id: String,
    /// LLM calls at once inside one layer.
    pub max_parallel: usize,
    /// Why the shift must stop (the STOP file, `until`), checked between steps.
    pub stop: Box<dyn Fn() -> Option<String> + Sync>,
}
