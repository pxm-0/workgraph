//! `~/.workgraph/shift.lock` (design §7.3 step 1): an exclusive `flock` held for the life of the
//! shift, with the holder's pid and start time written inside. The OS releases the lock when the
//! process ends, however it ends, so taking it is atomic and never races a stale-lock cleanup.
//! A lock file that still names a pid when it is taken was left by a shift that died: that holder
//! is reported as stale. The file is never removed, only emptied on a clean release.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Debug)]
pub struct Lock {
    file: File,
}

/// Whether process `pid` is alive (`kill -0`).
pub fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub(crate) fn first_pid(text: &str) -> Option<u32> {
    text.split_whitespace().next()?.parse().ok()
}

/// The pid in a lock file, if it holds one.
pub fn holder(path: &Path) -> Option<u32> {
    first_pid(&std::fs::read_to_string(path).ok()?)
}

/// What taking the lock found.
#[derive(Debug)]
pub enum Taken {
    Fresh(Lock),
    /// The previous holder died without releasing; its pid (0 when unreadable) is reported.
    Stale(Lock, u32),
    /// A live shift holds it (pid 0 while that shift has not written its pid yet).
    Held(u32),
}

pub fn take(path: &Path) -> Result<Taken, String> {
    if let Some(dir) = path.parent() {
        crate::state_dirs::private_dir(dir)?;
    }
    let what = |e: std::io::Error| format!("cannot use the lock {}: {e}", path.display());
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(what)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Taken::Held(holder(path).unwrap_or(0))),
        Err(TryLockError::Error(e)) => return Err(what(e)),
    }
    let mut old = String::new();
    file.read_to_string(&mut old).map_err(what)?;
    file.set_len(0).map_err(what)?;
    file.seek(SeekFrom::Start(0)).map_err(what)?;
    writeln!(file, "{} {}", std::process::id(), crate::journal::now()).map_err(what)?;
    file.sync_all().map_err(what)?;
    let lock = Lock { file };
    Ok(if old.trim().is_empty() {
        Taken::Fresh(lock)
    } else {
        Taken::Stale(lock, first_pid(&old).unwrap_or(0))
    })
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Best effort: an unemptied file only makes the next shift report a stale holder. The
        // flock itself is released when the file closes.
        let _ = self.file.set_len(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `take` once a release is done. A child that a parallel test forks holds a copy of every
    /// descriptor until it execs (they are close-on-exec), so a release can lag a few ms.
    fn take_released(p: &Path) -> Taken {
        for _ in 0..200 {
            match take(p).unwrap() {
                Taken::Held(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                t => return t,
            }
        }
        panic!("the lock was never released");
    }

    #[test]
    fn fresh_held_and_stale_locks() {
        let dir = std::env::temp_dir().join(format!("wg-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("shift.lock");
        let first = take(&p).unwrap();
        assert!(matches!(first, Taken::Fresh(_)));
        assert!(matches!(take(&p).unwrap(), Taken::Held(pid) if pid == std::process::id()));
        drop(first);
        assert_eq!(holder(&p), None, "a clean release empties the lock");
        assert!(matches!(take_released(&p), Taken::Fresh(_)));
        std::fs::write(&p, "999999999 1\n").unwrap();
        let stale = take_released(&p);
        assert!(matches!(stale, Taken::Stale(_, 999999999)));
        assert_eq!(holder(&p), Some(std::process::id()));
        drop(stale);
        std::fs::write(&p, "garbage").unwrap();
        assert!(matches!(take_released(&p), Taken::Stale(_, 0)));
    }

    #[test]
    fn concurrent_takers_of_a_stale_lock_get_one_lock_between_them() {
        let dir = std::env::temp_dir().join(format!("wg-lock-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shift.lock");
        std::fs::write(&p, "999999999 1\n").unwrap();
        let barrier = std::sync::Barrier::new(8);
        let got: Vec<Taken> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        take(&p).unwrap()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let holders = got.iter().filter(|t| !matches!(t, Taken::Held(_))).count();
        assert_eq!(holders, 1, "{got:?}");
        drop(got);
        assert!(matches!(take_released(&p), Taken::Fresh(_)));
    }
}
