//! The engine driver: runs the factory's step loop, validates every command, and executes it.

pub mod command_check;
pub mod convert;
pub mod driver;
pub mod kernel;

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
}
