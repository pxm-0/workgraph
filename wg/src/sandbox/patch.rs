//! A patch returned by the sandbox (design §8.5 step 5) is untrusted: decoded, at most 2 MB, every
//! path `git apply --numstat -z` lists must be one of the unit's files, then `--check`, then apply.
//! Anything else is refused and reported to the fixer.

use super::job::MAX_PATCH;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let bad = || "the patch is not base64".to_string();
    let t = text.trim_end_matches('=');
    if text.len() % 4 != 0 || text.len() - t.len() > 2 {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(t.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in t.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c).ok_or_else(bad)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

fn git_apply(root: &Path, args: &[&str], patch: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .arg("apply")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git apply failed to start: {e}"))?;
    if let Some(mut w) = child.stdin.take() {
        w.write_all(patch)
            .map_err(|e| format!("cannot hand the patch to git apply: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git apply failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git apply {} refused the patch: {}",
            args.join(" "),
            crate::ids::one_line(&String::from_utf8_lossy(&out.stderr), 300)
        ));
    }
    Ok(out.stdout)
}

/// Paths `git apply --numstat -z` names: renames list both sides.
fn touched(numstat: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut fields = numstat.split(|&b| b == 0).peekable();
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(rec).map_err(|_| "a patch path is not UTF-8")?;
        let mut parts = text.splitn(3, '\t');
        let (_, _, path) = (parts.next(), parts.next(), parts.next().unwrap_or(""));
        if path.is_empty() {
            // `added\tdeleted\t` then NUL, then source NUL, then destination NUL.
            for _ in 0..2 {
                let p = fields
                    .next()
                    .filter(|p| !p.is_empty())
                    .ok_or("a rename record is cut short")?;
                out.push(String::from_utf8_lossy(p).into_owned());
            }
        } else {
            out.push(path.to_string());
        }
    }
    Ok(out)
}

/// A file is the unit's if it is one of `files` or under one ending in `/`.
pub fn in_unit(path: &str, files: &[String]) -> bool {
    files.iter().any(|f| match f.strip_suffix('/') {
        Some(dir) => path.starts_with(&format!("{dir}/")),
        None => path == f,
    })
}

/// Applies `encoded` in `root` if every rule holds; returns the paths it changed.
pub fn apply(root: &Path, encoded: &str, files: &[String]) -> Result<Vec<String>, String> {
    if encoded.len() > MAX_PATCH * 4 / 3 + 8 {
        return Err("the patch is over 2 MB".into());
    }
    let patch = decode(encoded)?;
    if patch.len() > MAX_PATCH {
        return Err("the patch is over 2 MB".into());
    }
    let paths = touched(&git_apply(root, &["--numstat", "-z"], &patch)?)?;
    if paths.is_empty() {
        return Err("the patch changes nothing".into());
    }
    if let Some(p) = paths
        .iter()
        .find(|p| !in_unit(p, files) || p.split('/').any(|c| c == ".." || c == ".git"))
    {
        return Err(format!("the patch touches {p}, which is not the unit's"));
    }
    git_apply(root, &["--check"], &patch)?;
    git_apply(root, &[], &patch)?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_and_rejects_junk() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(decode(&encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(b"fo"), "Zm8=");
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        assert!(decode("Zm9").is_err() && decode("Zm!=").is_err() && decode("Z===").is_err());
    }

    #[test]
    fn only_the_units_files_may_change() {
        let d = std::env::temp_dir().join(format!("wg-patch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("db")).unwrap();
        let sh = |c: &str| {
            assert!(
                Command::new("sh")
                    .args(["-c", c])
                    .current_dir(&d)
                    .status()
                    .unwrap()
                    .success(),
                "{c}"
            )
        };
        sh("git init -q && git config user.email t@e.invalid && git config user.name t && echo a > db/1.sql && echo x > other.txt && git add -A && git commit -qm i");
        let diff = |f: &str, body: &str| {
            sh(&format!("printf '{body}' > {f}"));
            let out = Command::new("git")
                .args(["diff", "--binary"])
                .current_dir(&d)
                .output()
                .unwrap()
                .stdout;
            sh("git checkout -q -- .");
            encode(&out)
        };
        let unit = vec!["db/".to_string()];
        let good = diff("db/1.sql", "a\\nb\\n");
        let bad = diff("other.txt", "y\\n");
        assert!(apply(&d, &bad, &unit).unwrap_err().contains("other.txt"));
        assert_eq!(apply(&d, &good, &unit).unwrap(), ["db/1.sql"]);
        assert_eq!(
            std::fs::read_to_string(d.join("db/1.sql")).unwrap(),
            "a\nb\n"
        );
        assert!(
            apply(&d, &good, &unit).unwrap_err().contains("--check"),
            "applying twice fails the check"
        );
        assert!(apply(&d, "!!!!", &unit).is_err());
        assert!(apply(&d, &"A".repeat(MAX_PATCH * 2), &unit)
            .unwrap_err()
            .contains("2 MB"));
        assert!(in_unit("db/x/y.sql", &unit) && !in_unit("dbx/y.sql", &unit));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn renames_name_both_sides() {
        assert_eq!(touched(b"1\t0\tsrc/a.rs\0").unwrap(), ["src/a.rs"]);
        assert_eq!(
            touched(b"0\t0\t\0old.sql\0new.sql\0").unwrap(),
            ["old.sql", "new.sql"]
        );
        assert!(touched(b"0\t0\t\0old.sql\0").is_err());
    }
}
