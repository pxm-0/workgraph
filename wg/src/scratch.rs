//! Scratch wg makes for one run: its tmp dir under `~/.workgraph-work/tmp`, and the doctor
//! probe's run dirs. Each goes when its run ends; `wg shift gc` sweeps what a crash left.

use crate::shift::lock;
use crate::state_dirs::{private_dir, Dirs};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The newest doctor probes kept for inspection.
pub const DOCTOR_KEEP: usize = 3;
const TMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// A run's tmp dir, removed when dropped: on success, error and early return alike.
pub struct RunTmp {
    root: PathBuf,
    path: PathBuf,
}

impl RunTmp {
    pub fn create(dirs: &Dirs, run: &str) -> Result<RunTmp, String> {
        let path = dirs.run_tmp(run);
        private_dir(&path)?;
        Ok(RunTmp {
            root: tmp_root(dirs),
            path,
        })
    }
}

impl Drop for RunTmp {
    fn drop(&mut self) {
        // Logged, never fatal: a leftover dir only waits for `wg shift gc`.
        if let Err(e) = remove(&self.root, &self.path) {
            eprintln!("wg: cannot remove the run tmp dir: {e}");
        }
    }
}

fn tmp_root(dirs: &Dirs) -> PathBuf {
    dirs.work().join("tmp")
}

/// Removes `path`, only when it is a dir strictly under `root`; a missing path is already gone.
fn remove(root: &Path, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let p = crate::git::under(root, path)?;
    std::fs::remove_dir_all(&p).map_err(|e| format!("cannot remove {}: {e}", p.display()))
}

/// A live pid holding the shift lock or a memory lock. Locks name a pid, not a run, so any live
/// holder may own any tmp dir.
fn live_holder(dirs: &Dirs) -> Option<u32> {
    let memory_locks = std::fs::read_dir(dirs.memory_root())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lock"));
    std::iter::once(dirs.shift_lock())
        .chain(memory_locks)
        .filter_map(|p| lock::holder(&p))
        .find(|pid| lock::alive(*pid))
}

fn modified(p: &Path) -> Result<SystemTime, String> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{}: {e}", p.display()))
}

/// Entries of `dir` with their mtimes; a missing `dir` has none.
fn entries(dir: &Path) -> Result<Vec<(PathBuf, SystemTime)>, String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
    };
    rd.map(|e| {
        let p = e
            .map_err(|e| format!("cannot list {}: {e}", dir.display()))?
            .path();
        Ok((p.clone(), modified(&p)?))
    })
    .collect()
}

/// Removes `paths` under `root`, reporting each as removed or with its error.
fn remove_all(root: &Path, paths: Vec<PathBuf>) -> Value {
    let (removed, errors): (Vec<_>, Vec<_>) = paths
        .into_iter()
        .map(|p| remove(root, &p).map(|_| p.display().to_string()))
        .partition(Result::is_ok);
    json!({
        "removed": removed.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
        "errors": errors.into_iter().map(Result::unwrap_err).collect::<Vec<_>>(),
    })
}

/// Tmp dirs older than a day, left by runs that died. Skipped while a run or shift is live.
pub fn sweep_tmp(dirs: &Dirs) -> Value {
    if let Some(pid) = live_holder(dirs) {
        return json!({ "skipped": format!("pid {pid} holds a wg lock") });
    }
    let root = tmp_root(dirs);
    let now = SystemTime::now();
    match entries(&root) {
        Ok(es) => remove_all(
            &root,
            es.into_iter()
                .filter(|(_, m)| now.duration_since(*m).is_ok_and(|age| age > TMP_MAX_AGE))
                .map(|(p, _)| p)
                .collect(),
        ),
        Err(e) => json!({ "removed": [], "errors": [e] }),
    }
}

/// Removes all but the newest `DOCTOR_KEEP` doctor probe run dirs.
pub fn prune_doctor(dirs: &Dirs) -> Value {
    let root = dirs.state().join("runs");
    match entries(&root) {
        Ok(es) => {
            let mut probes: Vec<_> = es
                .into_iter()
                .filter(|(p, _)| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("doctor-"))
                })
                .collect();
            probes.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
            remove_all(
                &root,
                probes
                    .into_iter()
                    .skip(DOCTOR_KEEP)
                    .map(|(p, _)| p)
                    .collect(),
            )
        }
        Err(e) => json!({ "removed": [], "errors": [e] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> Dirs {
        let h = std::env::temp_dir().join(format!("wg-scratch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        Dirs { home: h }
    }

    fn dir_aged(p: &Path, age: Duration) {
        std::fs::create_dir_all(p).unwrap();
        std::fs::File::open(p)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn run_tmp_is_removed_when_dropped() {
        let d = home("drop");
        let t = RunTmp::create(&d, "r-1").unwrap();
        assert!(d.run_tmp("r-1").is_dir());
        drop(t);
        assert!(!d.run_tmp("r-1").exists());
        assert!(d.work().join("tmp").is_dir(), "only the run's own dir goes");
        let _ = std::fs::remove_dir_all(&d.home);
    }

    #[test]
    fn sweep_keeps_everything_under_a_live_lock_and_old_dirs_go_without_one() {
        let d = home("sweep");
        let (old, fresh) = (d.run_tmp("r-old"), d.run_tmp("r-fresh"));
        dir_aged(&old, 2 * DAY);
        dir_aged(&fresh, Duration::from_secs(60));
        std::fs::create_dir_all(d.memory_root()).unwrap();
        let mem_lock = d.memory_root().join("o__r.lock");
        std::fs::write(&mem_lock, format!("{}\n", std::process::id())).unwrap();

        let out = sweep_tmp(&d);
        assert!(out["skipped"].is_string(), "{out}");
        assert!(old.exists() && fresh.exists());

        // A dead holder is no lock: the old dir goes, the fresh one stays.
        std::fs::write(&mem_lock, "999999999\n").unwrap();
        let out = sweep_tmp(&d);
        assert_eq!(out["removed"].as_array().map(Vec::len), Some(1), "{out}");
        assert_eq!(out["errors"], json!([]));
        assert!(!old.exists() && fresh.exists());
        let _ = std::fs::remove_dir_all(&d.home);
    }

    #[test]
    fn doctor_keeps_the_newest_three_probes_and_no_other_run() {
        let d = home("doctor");
        for i in 0..5u64 {
            dir_aged(
                &d.run(&format!("doctor-{i}")),
                Duration::from_secs(60 * (i + 1)),
            );
        }
        dir_aged(&d.run("r-old"), 30 * DAY);
        let out = prune_doctor(&d);
        assert_eq!(out["errors"], json!([]), "{out}");
        let left: Vec<bool> = (0..5)
            .map(|i| d.run(&format!("doctor-{i}")).exists())
            .collect();
        assert_eq!(left, [true, true, true, false, false]);
        assert!(d.run("r-old").exists());
        let _ = std::fs::remove_dir_all(&d.home);
    }

    #[test]
    fn removal_refuses_paths_outside_its_root() {
        let d = home("guard");
        let outside = d.home.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(d.work().join("tmp")).unwrap();
        assert!(remove(&d.work().join("tmp"), &outside).is_err());
        assert!(outside.exists());
        let _ = std::fs::remove_dir_all(&d.home);
    }
}
