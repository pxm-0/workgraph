//! The work, as a throwaway tar copy (design §8.5 step 1): every file `git ls-files -co
//! --exclude-standard` lists, so no commit is needed. No `.git`, no ignored files, no `.env*`
//! except `.env.example`. Entries are owned by uid 1000, the step user, with mtime 0.

use std::path::Path;

/// The tar the agent imports, at most this big.
pub const MAX_BUNDLE: usize = 256 * 1024 * 1024;

/// Paths that never leave the Mac.
pub fn excluded(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel == ".git"
        || rel.starts_with(".git/")
        || rel.contains("/.git/")
        || (name.starts_with(".env") && name != ".env.example")
}

fn octal(field: &mut [u8], v: u64) {
    let s = format!("{v:0w$o}", w = field.len() - 1);
    field[..s.len()].copy_from_slice(s.as_bytes());
}

/// One ustar header; long names split into prefix and name at a `/`.
fn header(path: &str, size: u64, mode: u32, kind: u8, link: &str) -> Result<[u8; 512], String> {
    let mut h = [0u8; 512];
    let (prefix, name) = if path.len() <= 100 {
        ("", path)
    } else {
        let cut = path[..path.len().min(156)]
            .rfind('/')
            .filter(|&i| path.len() - i - 1 <= 100)
            .ok_or_else(|| format!("path too long for the bundle: {path}"))?;
        (&path[..cut], &path[cut + 1..])
    };
    if link.len() > 100 {
        return Err(format!("symlink target too long for the bundle: {path}"));
    }
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 1000);
    octal(&mut h[116..124], 1000);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    Ok(h)
}

fn pad(out: &mut Vec<u8>) {
    let rem = out.len() % 512;
    if rem != 0 {
        out.resize(out.len() + 512 - rem, 0);
    }
}

/// A tar of `files` (repo-relative) under `root`; missing files (deleted in the tree) are skipped.
pub fn tar(root: &Path, files: &[String]) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for rel in files.iter().filter(|f| !excluded(f)) {
        if rel.starts_with('/') || rel.split('/').any(|p| p == ".." || p.is_empty()) {
            return Err(format!("git listed an unsafe path: {rel}"));
        }
        let p = root.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        let parents: Vec<&str> = rel.split('/').collect();
        for i in 1..parents.len() {
            let d = parents[..i].join("/");
            if !dirs.contains(&d) {
                out.extend_from_slice(&header(&format!("{d}/"), 0, 0o755, b'5', "")?);
                dirs.push(d);
            }
        }
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&p).map_err(|e| format!("{rel}: {e}"))?;
            let target = target
                .to_str()
                .ok_or_else(|| format!("{rel}: link is not UTF-8"))?;
            out.extend_from_slice(&header(rel, 0, 0o777, b'2', target)?);
        } else if meta.is_file() {
            let body = std::fs::read(&p).map_err(|e| format!("cannot read {rel}: {e}"))?;
            let mode = if meta.permissions().mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            out.extend_from_slice(&header(rel, body.len() as u64, mode, b'0', "")?);
            out.extend_from_slice(&body);
            pad(&mut out);
        }
        if out.len() > MAX_BUNDLE {
            return Err("the work is over 256 MB; the sandbox takes only tracked sources".into());
        }
    }
    out.extend_from_slice(&[0u8; 1024]);
    Ok(out)
}

/// Tracked and untracked-but-not-ignored files of the worktree at `root`.
pub fn listed(root: &Path) -> Result<Vec<String>, String> {
    let out = std::process::Command::new("git")
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git ls-files failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut files = Vec::new();
    for raw in out.stdout.split(|&b| b == 0).filter(|b| !b.is_empty()) {
        let s = std::str::from_utf8(raw).map_err(|_| "a path is not UTF-8".to_string())?;
        if !files.iter().any(|f: &String| f == s) {
            files.push(s.to_string());
        }
    }
    Ok(files)
}

/// The stream `wg sandbox-agent submit` reads: the job line, then the tar.
pub fn stream(job: &super::job::Job, root: &Path) -> Result<Vec<u8>, String> {
    let mut out = serde_json::to_vec(job).map_err(|e| e.to_string())?;
    out.push(b'\n');
    out.extend(tar(root, &listed(root)?)?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(t: &[u8]) -> Vec<(String, char, u64)> {
        let mut out = vec![];
        let mut i = 0;
        while i + 512 <= t.len() && t[i] != 0 {
            let h = &t[i..i + 512];
            let s = |a: usize, b: usize| {
                String::from_utf8_lossy(&h[a..b])
                    .trim_end_matches('\0')
                    .to_string()
            };
            let size = u64::from_str_radix(s(124, 135).trim(), 8).unwrap();
            let prefix = s(345, 500);
            let name = if prefix.is_empty() {
                s(0, 100)
            } else {
                format!("{prefix}/{}", s(0, 100))
            };
            let sum: u32 = h
                .iter()
                .enumerate()
                .map(|(k, &b)| {
                    if (148..156).contains(&k) {
                        32
                    } else {
                        u32::from(b)
                    }
                })
                .sum();
            assert_eq!(
                u32::from_str_radix(s(148, 154).trim(), 8).unwrap(),
                sum,
                "checksum of {name}"
            );
            assert_eq!(s(108, 115), "0001750", "uid 1000");
            out.push((name, h[156] as char, size));
            i += 512 + size.div_ceil(512) as usize * 512;
        }
        out
    }

    #[test]
    fn secrets_and_git_stay_on_the_mac() {
        for p in [
            ".git",
            ".git/config",
            "sub/.git/x",
            ".env",
            "app/.env.local",
        ] {
            assert!(excluded(p), "{p}");
        }
        for p in [".env.example", "src/env.js", ".github/x.yml", "a.gitignore"] {
            assert!(!excluded(p), "{p}");
        }
    }

    #[test]
    fn a_worktree_bundles_tracked_and_new_files_only() {
        let d = std::env::temp_dir().join(format!("wg-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src/deep")).unwrap();
        let sh = |c: &str| {
            assert!(
                std::process::Command::new("sh")
                    .args(["-c", c])
                    .current_dir(&d)
                    .status()
                    .unwrap()
                    .success(),
                "{c}"
            )
        };
        sh("git init -q && git config user.email t@e.invalid && git config user.name t");
        std::fs::write(d.join("a.txt"), "tracked\n").unwrap();
        std::fs::write(d.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(d.join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(d.join(".env.example"), "SECRET=\n").unwrap();
        std::fs::write(d.join("ignored.txt"), "x").unwrap();
        let long = format!("src/deep/{}.txt", "n".repeat(95));
        std::fs::write(d.join(&long), "long").unwrap();
        sh("git add -A && git commit -qm i && echo new > new.txt && ln -s a.txt link");
        let t = tar(&d, &listed(&d).unwrap()).unwrap();
        let names: Vec<String> = entries(&t).into_iter().map(|e| e.0).collect();
        for want in [
            "a.txt",
            ".gitignore",
            ".env.example",
            "new.txt",
            "link",
            "src/",
            "src/deep/",
            long.as_str(),
        ] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        for not in [".env", "ignored.txt", ".git/config"] {
            assert!(!names.iter().any(|n| n == not), "{not}");
        }
        assert!(entries(&t)
            .iter()
            .any(|(n, k, s)| n == "link" && *k == '2' && *s == 0));
        assert!(header(&format!("{}/x", "d".repeat(200)), 0, 0o644, b'0', "").is_err());
        assert!(tar(&d, &["../etc/passwd".into()]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
