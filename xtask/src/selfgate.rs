//! The self-gate checks (design §12.2, §12.3) as CI runs them: `protected-paths` diffs base...head
//! with rename detection off and checks every side of every entry, plus the caps, against the
//! protected list compiled into this (base) build; `lint-editable` runs the token lint over every
//! editable Rust file of the tree. Both share the kernel's code with `wg self gate`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use wg::selfimprove::protected::{self, Verdict};

fn flag<'a>(args: &[&'a str], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| *a == name)
        .and_then(|i| args.get(i + 1).copied())
}

fn repo(args: &[&str]) -> PathBuf {
    PathBuf::from(flag(args, "--repo").unwrap_or("."))
}

fn fail(what: &str, why: &str) -> ExitCode {
    eprintln!("{what}: {why}");
    ExitCode::FAILURE
}

/// What the diff of `base...head` in `repo` allows under `rules`.
pub fn check(
    repo: &Path,
    rules: &protected::Rules,
    base: &str,
    head: &str,
) -> Result<Verdict, String> {
    let range = format!("{base}...{head}");
    let z = wg::git::git(
        repo,
        &["diff", "--no-renames", "--name-status", "-z", &range],
        &[],
    )?;
    let numstat = wg::git::git(
        repo,
        &["diff", "--no-renames", "--numstat", "-z", &range],
        &[],
    )?;
    let changes = protected::parse_name_status(&z)?;
    Ok(protected::verdict(
        rules,
        &changes,
        protected::changed_lines(&numstat),
    ))
}

pub fn protected_paths(args: &[&str]) -> ExitCode {
    let (Some(base), Some(head)) = (flag(args, "--base"), flag(args, "--head")) else {
        return fail("protected-paths", "--base and --head are required");
    };
    match check(&repo(args), &protected::compiled(), base, head) {
        Ok(Verdict::Refused(why)) => fail("protected-paths", &why.join("\n  ")),
        Ok(Verdict::Held(files)) => {
            println!(
                "protected-paths: editable, held for the owner (D2): {}",
                files.join(", ")
            );
            ExitCode::SUCCESS
        }
        Ok(Verdict::AutoMergeable) => {
            println!("protected-paths: every change is editable and may auto-merge");
            ExitCode::SUCCESS
        }
        Err(e) => fail("protected-paths", &e),
    }
}

/// Findings over every editable Rust file under `repo`, as `path line N: why`.
pub fn lint(repo: &Path) -> Result<Vec<String>, String> {
    let root = repo.join("wg-factory/src");
    let mut pending = vec![root.clone()];
    let mut out = Vec::new();
    while let Some(d) = pending.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| format!("cannot read {}: {e}", d.display()))? {
            let p = e.map_err(|e| e.to_string())?.path();
            if p.is_dir() {
                pending.push(p);
                continue;
            }
            let rel = format!(
                "wg-factory/src/{}",
                p.strip_prefix(&root).unwrap_or(&p).display()
            );
            if wg::selfimprove::lint::covers(&rel) {
                let src = std::fs::read_to_string(&p).map_err(|e| format!("{rel}: {e}"))?;
                out.extend(
                    wg::selfimprove::lint::findings(&src)
                        .into_iter()
                        .map(|f| format!("{rel} {f}")),
                );
            }
        }
    }
    out.sort();
    Ok(out)
}

pub fn lint_editable(args: &[&str]) -> ExitCode {
    match lint(&repo(args)) {
        Ok(f) if f.is_empty() => {
            println!("lint-editable: no compile-time route out of the editable crate");
            ExitCode::SUCCESS
        }
        Ok(f) => fail("lint-editable", &f.join("\n  ")),
        Err(e) => fail("lint-editable", &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(ok.success(), "{script}");
    }

    #[test]
    fn protected_paths_and_lint_over_a_real_diff() {
        let d = std::env::temp_dir().join(format!("xtask-selfgate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("wg-factory/src/plan")).unwrap();
        std::fs::create_dir_all(d.join("wg/tests")).unwrap();
        std::fs::write(
            d.join("wg-factory/src/lib.rs"),
            "#![no_std]\nextern crate alloc;\n",
        )
        .unwrap();
        std::fs::write(d.join("wg-factory/src/plan/mod.rs"), "pub fn p() {}\n").unwrap();
        std::fs::write(d.join("wg/tests/flow.rs"), "// t\n").unwrap();
        sh(&d, "git init -q -b main && git config user.name t && git config user.email t@x.invalid && git add -A && git commit -qm i && git tag base");
        let rules = protected::compiled();
        sh(&d, "git checkout -q -b docs && mkdir docs && echo a > docs/a.md && git add -A && git commit -qm d");
        assert_eq!(
            check(&d, &rules, "base", "docs").unwrap(),
            Verdict::AutoMergeable
        );
        sh(&d, "git checkout -q -b mv base && mkdir -p wg-factory/tests && git mv wg/tests/flow.rs wg-factory/tests/flow.rs && git commit -qm m");
        assert!(
            matches!(check(&d, &rules, "base", "mv").unwrap(), Verdict::Refused(w) if w[0].contains("wg/tests/flow.rs"))
        );
        assert!(check(&d, &rules, "base", "nope").is_err());
        assert!(lint(&d).unwrap().is_empty(), "lib.rs is not linted");
        std::fs::write(
            d.join("wg-factory/src/plan/mod.rs"),
            "pub fn p() { let _ = env!(\"HOME\"); }\n",
        )
        .unwrap();
        assert_eq!(lint(&d).unwrap(), ["wg-factory/src/plan/mod.rs line 1: `env!` reaches outside the crate at compile time"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
