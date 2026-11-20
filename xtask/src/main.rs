//! Repo checks, run as `cargo xtask <check>` (alias in `.cargo/config.toml`).
//!
//!   cargo xtask size                                    fail on any .rs file over 800 lines
//!   cargo xtask protected-paths --base B --head H [--repo D]   a self PR's diff (design §12.3)
//!   cargo xtask lint-editable [--repo D]                editable Rust's compile-time routes
//!
//! self-gate.yml runs the BASE checkout's xtask against the head tree, so a PR cannot change the
//! rules it is judged by.

mod selfgate;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const MAX_LINES: usize = 800;
/// Directories never scanned: build output, VCS data, dependencies.
const SKIP_DIRS: [&str; 3] = ["target", ".git", "node_modules"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["size"] => size(&repo_root()),
        ["protected-paths", rest @ ..] => selfgate::protected_paths(rest),
        ["lint-editable", rest @ ..] => selfgate::lint_editable(rest),
        _ => {
            eprintln!("usage: cargo xtask size | protected-paths --base B --head H [--repo D] | lint-editable [--repo D]");
            ExitCode::from(2)
        }
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the repo root")
        .to_path_buf()
}

fn size(root: &Path) -> ExitCode {
    match oversized(root) {
        Ok(over) if over.is_empty() => {
            println!("size: every .rs file is at most {MAX_LINES} lines");
            ExitCode::SUCCESS
        }
        Ok(over) => {
            for (path, lines) in &over {
                eprintln!(
                    "size: {} has {lines} lines (max {MAX_LINES})",
                    path.display()
                );
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("size: cannot scan {}: {e}", root.display());
            ExitCode::FAILURE
        }
    }
}

/// Every `.rs` file under `root` with more than `MAX_LINES` lines, paths relative to `root`, sorted.
fn oversized(root: &Path) -> io::Result<Vec<(PathBuf, usize)>> {
    let mut over = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() && !SKIP_DIRS.iter().any(|s| entry.file_name() == *s) {
                pending.push(path);
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "rs") {
                let lines = fs::read_to_string(&path)?.lines().count();
                if lines > MAX_LINES {
                    over.push((
                        path.strip_prefix(root).unwrap_or(&path).to_path_buf(),
                        lines,
                    ));
                }
            }
        }
    }
    over.sort();
    Ok(over)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_rs_files_over_the_limit_outside_skipped_dirs() {
        let root = std::env::temp_dir().join(format!("xtask-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["src/deep", "target", "node_modules"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let lines = |n: usize| "x\n".repeat(n);
        fs::write(root.join("src/ok.rs"), lines(MAX_LINES)).unwrap();
        fs::write(root.join("src/deep/big.rs"), lines(MAX_LINES + 1)).unwrap();
        fs::write(root.join("src/notes.txt"), lines(MAX_LINES + 1)).unwrap();
        fs::write(root.join("target/gen.rs"), lines(MAX_LINES + 1)).unwrap();
        fs::write(root.join("node_modules/dep.rs"), lines(MAX_LINES + 1)).unwrap();

        assert_eq!(
            oversized(&root).unwrap(),
            [(PathBuf::from("src/deep/big.rs"), MAX_LINES + 1)]
        );
        assert!(
            oversized(&root.join("missing")).is_err(),
            "a scan error is reported, not skipped"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
