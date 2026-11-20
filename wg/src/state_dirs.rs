//! The owner-local layout. `~/.workgraph/` holds state the kernel trusts and is denied to every
//! agent; `~/.workgraph-work/` holds what agents may touch (temp dirs, later worktrees).

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Dirs {
    pub home: PathBuf,
}

impl Dirs {
    /// From `$HOME`, which must be absolute.
    pub fn from_env() -> Result<Dirs, String> {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err("HOME must be an absolute path".into());
        }
        Ok(Dirs { home })
    }

    pub fn state(&self) -> PathBuf {
        self.home.join(".workgraph")
    }

    pub fn work(&self) -> PathBuf {
        self.home.join(".workgraph-work")
    }

    pub fn config(&self) -> PathBuf {
        self.state().join("factory.toml")
    }

    pub fn run(&self, run: &str) -> PathBuf {
        self.state().join("runs").join(run)
    }

    pub fn run_tmp(&self, run: &str) -> PathBuf {
        self.work().join("tmp").join(run)
    }

    pub fn memory(&self, slug: &str) -> PathBuf {
        self.state().join("memory").join(format!("{slug}.json"))
    }

    pub fn ledger(&self) -> PathBuf {
        self.state().join("cost.jsonl")
    }

    pub fn key(&self) -> PathBuf {
        self.state().join("key")
    }

    pub fn worktrees(&self) -> PathBuf {
        self.work().join("worktrees")
    }

    pub fn shifts(&self) -> PathBuf {
        self.state().join("shifts")
    }

    pub fn shift_lock(&self) -> PathBuf {
        self.state().join("shift.lock")
    }

    /// While this file exists no shift starts, and a running one stops between steps.
    pub fn stop_file(&self) -> PathBuf {
        self.state().join("STOP")
    }

    /// The record that links every run of one issue (design §7.4).
    pub fn task_record(&self, repo_slug: &str, issue: u64) -> PathBuf {
        self.state()
            .join("tasks")
            .join(format!("{repo_slug}#{issue}.json"))
    }

    /// The owner's answers for one issue, written only by `wg decide`.
    pub fn decisions(&self, repo_slug: &str, issue: u64) -> PathBuf {
        self.state()
            .join("decisions")
            .join(format!("{repo_slug}#{issue}.json"))
    }

    /// Label edits and issues the factory made, so intake never takes its own work.
    pub fn intake_ledger(&self) -> PathBuf {
        self.state().join("intake.jsonl")
    }

    /// The hash-chained self-ledger (design §12.1).
    pub fn self_ledger(&self) -> PathBuf {
        self.state().join("self-ledger.jsonl")
    }

    /// The self lane's switches: paused, merge enabled, denylisted SHAs (design §12.5).
    pub fn self_state(&self) -> PathBuf {
        self.state().join("self.json")
    }

    /// Installed binaries: `wg-<sha12>/wg`, with the `current` and `previous` symlinks.
    pub fn bin(&self) -> PathBuf {
        self.state().join("bin")
    }

    pub fn memory_root(&self) -> PathBuf {
        self.state().join("memory")
    }

    /// `memory/` as the binary `sha12` last left it, taken at install (design §10.1).
    pub fn memory_snapshot(&self, sha12: &str) -> PathBuf {
        self.state().join("memory-snapshots").join(sha12)
    }

    /// Throwaway clones for the eval and capture (design §12.4).
    pub fn eval_work(&self) -> PathBuf {
        self.work().join("eval")
    }

    pub fn claude_settings(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }
}

/// Creates `dir` (and parents) readable only by the owner.
pub fn private_dir(dir: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}
