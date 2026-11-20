//! Jobs on the sandbox, under `~wgsbx/jobs/<job>/` (design §8.5 step 2): `submit` stores the
//! stream and starts the job as a transient user unit, so cleanup belongs to the unit and not to
//! the Mac client; `poll` answers pending or the result; the unit's `run` and `cleanup` finish it.

use crate::sandbox::bundle::MAX_BUNDLE;
use crate::sandbox::job::{self, Job, JobResult, MAX_JOB_LINE};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The root-owned image allowlist (design §8.3); never a path from argv or the environment.
pub const IMAGES: &str = "/etc/wgsbx/images.toml";

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Allowlist {
    /// The doctor's probe image (busybox: sh, find, nc, grep).
    pub probe: String,
    pub images: Vec<String>,
}

impl Allowlist {
    pub fn parse(text: &str) -> Result<Allowlist, String> {
        let a: Allowlist = toml::from_str(text).map_err(|e| format!("{IMAGES}: {e}"))?;
        for i in a.images.iter().chain([&a.probe]) {
            job::image_ref(i)?;
        }
        Ok(a)
    }

    pub fn allows(&self, image: &str) -> bool {
        self.images.iter().any(|i| i == image)
    }
}

/// Owned by root and writable by nobody else, so an escape that lands as wgsbx cannot rewrite it.
pub fn root_owned(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is not root-owned and closed to others",
            path.display()
        ));
    }
    Ok(())
}

pub fn allowlist() -> Result<Allowlist, String> {
    root_owned(Path::new(IMAGES))?;
    let text =
        std::fs::read_to_string(IMAGES).map_err(|e| format!("not_provisioned: {IMAGES}: {e}"))?;
    Allowlist::parse(&text)
}

pub struct Store {
    pub root: PathBuf,
}

impl Store {
    /// `$HOME/jobs` of the sandbox user.
    pub fn home() -> Result<Store, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        if !home.starts_with('/') || home == "/" {
            return Err("HOME is not an absolute home directory".into());
        }
        let root = Path::new(&home).join("jobs");
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
        Ok(Store { root })
    }

    pub fn dir(&self, id: &str) -> Result<PathBuf, String> {
        job::job_id(id)?;
        Ok(self.root.join(id))
    }

    /// Reads the job line and the tar from `input`, validates the job, stores both.
    pub fn receive(
        &self,
        input: &mut dyn Read,
        allowed: &dyn Fn(&str) -> bool,
    ) -> Result<Job, String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = input
                .read(&mut byte)
                .map_err(|e| format!("cannot read the job: {e}"))?;
            if n == 0 || byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
            if line.len() > MAX_JOB_LINE {
                return Err("the job line is over 64 KB".into());
            }
        }
        let j: Job =
            serde_json::from_slice(&line).map_err(|e| format!("the job is not valid: {e}"))?;
        job::validate(&j, allowed)?;
        let dir = self.dir(&j.job)?;
        std::fs::create_dir(&dir).map_err(|e| format!("job {} cannot be stored: {e}", j.job))?;
        std::fs::write(
            dir.join("job.json"),
            serde_json::to_vec(&j).expect("serializes"),
        )
        .map_err(|e| format!("cannot store the job: {e}"))?;
        let mut tar = std::fs::File::create(dir.join("src.tar"))
            .map_err(|e| format!("cannot store the work: {e}"))?;
        let copied = std::io::copy(&mut input.take(MAX_BUNDLE as u64 + 1), &mut tar)
            .map_err(|e| format!("cannot store the work: {e}"))?;
        if copied > MAX_BUNDLE as u64 {
            return Err("the work is over 256 MB".into());
        }
        Ok(j)
    }

    pub fn load(&self, id: &str) -> Result<Job, String> {
        let text = std::fs::read(self.dir(id)?.join("job.json"))
            .map_err(|e| format!("no job {id}: {e}"))?;
        serde_json::from_slice(&text).map_err(|e| format!("job {id} is corrupt: {e}"))
    }

    pub fn tar(&self, id: &str) -> Result<PathBuf, String> {
        Ok(self.dir(id)?.join("src.tar"))
    }

    pub fn result(&self, id: &str) -> Result<Option<String>, String> {
        match std::fs::read_to_string(self.dir(id)?.join("result.json")) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read the result of {id}: {e}")),
        }
    }

    /// Atomic: a poll never sees half a result.
    pub fn save_result(&self, r: &JobResult) -> Result<(), String> {
        let dir = self.dir(&r.job)?;
        let tmp = dir.join("result.tmp");
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("cannot write the result: {e}"))?;
        f.write_all(serde_json::to_string(r).expect("serializes").as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("cannot write the result: {e}"))?;
        std::fs::rename(&tmp, dir.join("result.json"))
            .map_err(|e| format!("cannot write the result: {e}"))
    }

    /// Marks a job as running (it holds the lock), so a poll can tell it from one still queued.
    pub fn mark_started(&self, id: &str) -> Result<(), String> {
        std::fs::write(self.dir(id)?.join("started"), b"")
            .map_err(|e| format!("cannot mark job {id} started: {e}"))
    }

    pub fn started(&self, id: &str) -> Result<bool, String> {
        Ok(self.dir(id)?.join("started").exists())
    }

    /// Concurrency 1 (design §8.3): held for the whole job.
    pub fn lock(&self) -> Result<std::fs::File, String> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(".lock"))
            .map_err(|e| format!("cannot open the job lock: {e}"))?;
        f.lock()
            .map_err(|e| format!("cannot take the job lock: {e}"))?;
        Ok(f)
    }

    /// Job directories older than `secs`, for the reaper.
    pub fn older_than(&self, secs: u64) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return vec![];
        };
        rd.flatten()
            .filter(|e| job::job_id(&e.file_name().to_string_lossy()).is_ok())
            .filter(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > secs)
            })
            .map(|e| e.path())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::job::Cleanup;

    #[test]
    fn the_allowlist_takes_digests_only() {
        let d = "a".repeat(64);
        let ok = format!("probe = \"docker.io/library/busybox@sha256:{d}\"\nimages = [\"docker.io/library/node@sha256:{d}\"]\n");
        let a = Allowlist::parse(&ok).unwrap();
        assert!(a.allows(&format!("docker.io/library/node@sha256:{d}")) && !a.allows("x"));
        assert!(Allowlist::parse("probe = \"busybox:1\"\nimages = []\n").is_err());
        assert!(Allowlist::parse("images = []\n").is_err());
        let f = std::env::temp_dir().join(format!("wg-allow-{}", std::process::id()));
        std::fs::write(&f, "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(root_owned(&f).unwrap_err().contains("not root-owned"));
        assert!(root_owned(Path::new("/nonexistent/images.toml")).is_err());
    }

    #[test]
    fn receive_validates_stores_and_refuses_reuse() {
        let root = std::env::temp_dir().join(format!(
            "wg-store-{}-{}",
            std::process::id(),
            crate::journal::now_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let s = Store { root };
        let j = crate::sandbox::agent::run::tests::job(&[("t", "true")]);
        let mut stream = serde_json::to_vec(&j).unwrap();
        stream.push(b'\n');
        stream.extend_from_slice(b"TARBYTES");
        let yes = |_: &str| true;
        assert_eq!(s.receive(&mut stream.as_slice(), &yes).unwrap(), j);
        assert_eq!(std::fs::read(s.tar(&j.job).unwrap()).unwrap(), b"TARBYTES");
        assert_eq!(s.load(&j.job).unwrap(), j);
        assert!(s
            .receive(&mut stream.as_slice(), &yes)
            .unwrap_err()
            .contains("cannot be stored"));
        assert!(s
            .receive(&mut stream.as_slice(), &|_: &str| false)
            .unwrap_err()
            .contains("allowlist"));
        assert!(s.receive(&mut &b"{}\n"[..], &yes).is_err());
        assert_eq!(s.result(&j.job).unwrap(), None);
        assert!(
            !s.started(&j.job).unwrap(),
            "queued until it holds the lock"
        );
        s.mark_started(&j.job).unwrap();
        assert!(s.started(&j.job).unwrap());
        let r = JobResult {
            job: j.job.clone(),
            nonce: j.nonce.clone(),
            steps: vec![],
            patch: None,
            cleanup: Cleanup::default(),
            error: None,
        };
        s.save_result(&r).unwrap();
        assert!(s
            .result(&j.job)
            .unwrap()
            .unwrap()
            .contains("\"nonce\":\"n-1\""));
        assert!(s.dir("../etc").is_err());
        let _lock = s.lock().unwrap();
        assert!(s.older_than(3600).is_empty());
    }
}
