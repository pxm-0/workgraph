//! `~/.workgraph/shift.lock` (design §7.3 step 1): created with `create_new`, holding the pid and
//! start time. A lock whose pid is dead is stale: it is replaced, and the caller reports it.

use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

/// Whether process `pid` is alive (`kill -0`).
pub fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The pid in a lock file, if it holds one.
pub fn holder(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// What taking the lock found.
#[derive(Debug)]
pub enum Taken {
    Fresh(Lock),
    /// The previous holder died; its pid is reported.
    Stale(Lock, u32),
    /// A live shift holds it.
    Held(u32),
}

fn create(path: &Path) -> std::io::Result<Lock> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    writeln!(f, "{} {}", std::process::id(), crate::journal::now())?;
    Ok(Lock {
        path: path.to_path_buf(),
    })
}

pub fn take(path: &Path) -> Result<Taken, String> {
    if let Some(dir) = path.parent() {
        crate::state_dirs::private_dir(dir)?;
    }
    match create(path) {
        Ok(l) => return Ok(Taken::Fresh(l)),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(format!("cannot create {}: {e}", path.display()))
        }
        Err(_) => {}
    }
    match holder(path) {
        Some(pid) if alive(pid) => Ok(Taken::Held(pid)),
        old => {
            std::fs::remove_file(path)
                .map_err(|e| format!("cannot remove the stale lock {}: {e}", path.display()))?;
            let l = create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            Ok(Taken::Stale(l, old.unwrap_or(0)))
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Best effort: a lock left behind is detected as stale by the next shift.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_held_and_stale_locks() {
        let dir = std::env::temp_dir().join(format!("wg-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("shift.lock");
        let first = take(&p).unwrap();
        assert!(matches!(first, Taken::Fresh(_)));
        assert!(matches!(take(&p).unwrap(), Taken::Held(pid) if pid == std::process::id()));
        drop(first);
        assert!(!p.exists(), "dropping the lock removes it");
        std::fs::write(&p, "999999999 1\n").unwrap();
        let stale = take(&p).unwrap();
        assert!(matches!(stale, Taken::Stale(_, 999999999)));
        assert_eq!(holder(&p), Some(std::process::id()));
        drop(stale);
        std::fs::write(&p, "garbage").unwrap();
        assert!(matches!(take(&p).unwrap(), Taken::Stale(_, 0)));
    }
}
