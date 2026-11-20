//! Role + the kernel's own view → the permission allowlist and the guard manifest (design §5.2).
//! The factory names a role and a unit; it never carries a tool policy or a write root.

use crate::guard::bash::BashRules;
use crate::guard::hook::{Manifest, SandboxGrant};
use std::path::Path;
use wg_factory::api::Role;

/// What the kernel knows when it builds a call's policy.
pub struct Ctx<'a> {
    pub worktree: &'a Path,
    pub run_tmp: &'a Path,
    pub haruspex_cli: &'a str,
    pub task_id: Option<&'a str>,
    /// `.haruspex/` did not exist at run start, so the Reader may write AGENTS.md and CLAUDE.md.
    pub initializing: bool,
    /// Owner-approved check commands for this repo (project and factory strings).
    pub approved: &'a [String],
    pub secret_roots: Vec<String>,
    /// Risky or tainted work (Phase 3): the absolute `wg` and what its sandbox exec may do. Bash is
    /// then only `wg sandbox exec` and `wg sandbox poll` (design §5.2).
    pub sandbox: Option<(String, SandboxGrant)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub allowed: Vec<String>,
    pub disallowed: Vec<String>,
    pub manifest: Manifest,
}

/// Always disallowed, in `--disallowedTools` and (by omission) in the guard.
pub const ALWAYS_DENIED: &[&str] = &[
    "WebFetch",
    "WebSearch",
    "Task",
    "Agent",
    "Bash(gh:*)",
    "Bash(git push:*)",
    "Bash(git commit:*)",
    "Bash(git reset:*)",
    "Bash(git clean:*)",
    "Bash(git checkout --:*)",
    "Bash(curl:*)",
    "Bash(wget:*)",
    "Bash(ssh:*)",
    "Bash(orb:*)",
    "Bash(orbctl:*)",
    "Bash(sudo:*)",
    "Bash(security:*)",
    "Bash(env:*)",
    "Skill(deploy-oreochi)",
    "Skill(clasp-ship)",
];

const HX_SKILL: &str = "haruspex:haruspex";

fn abs(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `Edit(/<abs>)` and `Write(/<abs>)`: anchored patterns, since relative ones match the name at any
/// depth (ADR-001 §10b). Directories get `/**`.
fn write_entries(roots: &[String]) -> Vec<String> {
    roots
        .iter()
        .flat_map(|r| {
            let pat = match r.strip_suffix('/') {
                Some(dir) => format!("/{dir}/**"),
                None => format!("/{r}"),
            };
            [format!("Edit({pat})"), format!("Write({pat})")]
        })
        .collect()
}

fn bash_entries(b: &BashRules) -> Vec<String> {
    let mut out: Vec<String> = b.exact.iter().map(|e| format!("Bash({e})")).collect();
    out.extend(b.prefixes.iter().map(|p| format!("Bash({p}:*)")));
    if let Some(cli) = &b.haruspex_cli {
        let hx = format!("python3 {cli}");
        for h in &b.haruspex {
            out.push(match h.as_str() {
                "init" => format!("Bash({hx} init .)"),
                "task create" => format!("Bash({hx} task create:*)"),
                "task activate" => format!(
                    "Bash({hx} task activate {} .)",
                    b.task_id.as_deref().unwrap_or("")
                ),
                "gate" => format!("Bash({hx} gate ready-to-build . --json)"),
                _ => format!("Bash({hx} status:*)"),
            });
        }
    }
    if b.switch_branch {
        out.push("Bash(git switch -c:*)".into());
    }
    out
}

/// The policy for one call. `unit_files` are repo-relative paths from the kernel's own Reading.
pub fn policy(role: Role, unit_files: &[String], c: &Ctx) -> Policy {
    let wt = |rel: &str| abs(&c.worktree.join(rel));
    let tmp_dir = format!("{}/", abs(c.run_tmp));
    let task_file = c.task_id.map(|t| wt(&format!(".haruspex/tasks/{t}.json")));
    let mut bash = BashRules::default();
    let mut write_roots = Vec::new();
    let mut skills: Vec<&str> = Vec::new();
    let mut tools = vec!["Read", "Glob", "Grep", "Skill", "Edit", "Write", "Bash"];
    match role {
        Role::Reader => {
            bash.haruspex_cli = Some(c.haruspex_cli.to_string());
            bash.haruspex = ["init", "task create", "gate", "status"]
                .map(String::from)
                .to_vec();
            if c.task_id.is_some() {
                bash.haruspex.push("task activate".into());
                bash.task_id = c.task_id.map(str::to_string);
            }
            bash.exact = vec!["git status".into(), "git branch --show-current".into()];
            bash.switch_branch = true;
            write_roots.push(match &task_file {
                Some(f) => f.clone(),
                None => format!("{}/", wt(".haruspex/tasks")),
            });
            write_roots.push(wt(".haruspex/project.json"));
            write_roots.push(wt(".haruspex/reading.md"));
            if c.initializing {
                write_roots.push(wt("AGENTS.md"));
                write_roots.push(wt("CLAUDE.md"));
            }
            skills.push(HX_SKILL);
        }
        Role::Challenger => {
            tools.retain(|t| *t != "Write");
            bash.haruspex_cli = Some(c.haruspex_cli.to_string());
            bash.haruspex = vec!["gate".into()];
            write_roots.extend(task_file.clone());
            skills.push(HX_SKILL);
        }
        Role::Worker | Role::Fixer => {
            write_roots.extend(unit_files.iter().map(|f| match f.strip_suffix('/') {
                Some(dir) => format!("{}/", wt(dir)),
                None => wt(f),
            }));
            match &c.sandbox {
                Some((wg, g)) => {
                    let unit = g.unit.as_deref().unwrap_or("");
                    bash.prefixes = vec![
                        format!("{wg} sandbox exec --unit {unit}"),
                        format!("{wg} sandbox poll --job"),
                    ];
                }
                None => {
                    bash.prefixes = c.approved.to_vec();
                    write_roots.push(tmp_dir.clone());
                }
            }
            skills.extend(["mattpocock-skills:tdd", "mattpocock-skills:diagnosing-bugs"]);
        }
        Role::Reviewer => {
            bash.prefixes = match &c.sandbox {
                Some((wg, g)) => vec![
                    format!(
                        "{wg} sandbox exec --task {}",
                        g.task.as_deref().unwrap_or("")
                    ),
                    format!("{wg} sandbox poll --job"),
                ],
                None => c.approved.to_vec(),
            };
            bash.prefixes.push("git diff".into());
            bash.exact = vec!["git status".into()];
            write_roots.extend(task_file.clone());
            write_roots.push(tmp_dir.clone());
            skills.extend([HX_SKILL, "mattpocock-skills:code-review"]);
        }
        Role::Learner => tools.clear(),
    }
    let mut allowed: Vec<String> = tools
        .iter()
        .filter(|t| !["Skill", "Edit", "Write", "Bash"].contains(t))
        .map(|t| t.to_string())
        .collect();
    allowed.extend(skills.iter().map(|s| format!("Skill({s})")));
    allowed.extend(write_entries(&write_roots));
    allowed.extend(bash_entries(&bash));
    let mut read_roots = vec![abs(c.worktree)];
    if role != Role::Learner {
        read_roots.push(abs(c.run_tmp));
    }
    Policy {
        allowed,
        disallowed: ALWAYS_DENIED.iter().map(|s| s.to_string()).collect(),
        manifest: Manifest {
            label: String::new(),
            log: String::new(),
            worktree: abs(c.worktree),
            read_roots,
            write_roots,
            secret_roots: c.secret_roots.clone(),
            tools: tools.iter().map(|t| t.to_string()).collect(),
            skills: skills.iter().map(|s| s.to_string()).collect(),
            bash,
            sandbox: c.sandbox.as_ref().map(|(_, g)| g.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(approved: &'a [String]) -> Ctx<'a> {
        Ctx {
            worktree: Path::new("/r"),
            run_tmp: Path::new("/w/tmp/r-1"),
            haruspex_cli: "/h/haruspex.py",
            task_id: Some("T-001"),
            initializing: true,
            approved,
            secret_roots: vec!["/home/.workgraph".into()],
            sandbox: None,
        }
    }

    #[test]
    fn risky_and_tainted_work_gets_the_sandbox_only() {
        let approved = vec!["npm test".to_string()];
        let grant = |unit: Option<&str>, task: Option<&str>| SandboxGrant {
            repo: "o/r".into(),
            run: "r-1".into(),
            unit: unit.map(String::from),
            task: task.map(String::from),
            files: vec!["db/".into()],
        };
        let c = Ctx {
            sandbox: Some(("/opt/wg".into(), grant(Some("U2"), None))),
            ..ctx(&approved)
        };
        let w = policy(Role::Fixer, &["db/".into()], &c);
        assert_eq!(
            w.manifest.bash.prefixes,
            [
                "/opt/wg sandbox exec --unit U2",
                "/opt/wg sandbox poll --job"
            ]
        );
        assert_eq!(
            w.manifest.write_roots,
            ["/r/db/"],
            "no run tmp for a risky unit"
        );
        assert!(w
            .allowed
            .contains(&"Bash(/opt/wg sandbox exec --unit U2:*)".to_string()));
        assert!(
            !w.allowed.iter().any(|a| a.contains("npm test")),
            "no host check"
        );
        assert_eq!(w.manifest.sandbox.unwrap().unit.as_deref(), Some("U2"));
        let c = Ctx {
            sandbox: Some(("/opt/wg".into(), grant(None, Some("T-001")))),
            ..ctx(&approved)
        };
        let r = policy(Role::Reviewer, &[], &c);
        assert_eq!(
            r.manifest.bash.prefixes,
            [
                "/opt/wg sandbox exec --task T-001",
                "/opt/wg sandbox poll --job",
                "git diff"
            ]
        );
        assert!(!r.allowed.iter().any(|a| a.contains("npm test")));
    }

    #[test]
    fn worker_gets_its_files_tmp_and_approved_checks_only() {
        let approved = vec!["npm test".to_string()];
        let p = policy(
            Role::Worker,
            &["src/a.js".into(), "docs/".into()],
            &ctx(&approved),
        );
        assert_eq!(
            p.manifest.write_roots,
            ["/r/src/a.js", "/r/docs/", "/w/tmp/r-1/"]
        );
        assert!(p.allowed.contains(&"Edit(//r/src/a.js)".to_string()));
        assert!(p.allowed.contains(&"Write(//r/docs/**)".to_string()));
        assert!(p.allowed.contains(&"Bash(npm test:*)".to_string()));
        assert!(p
            .allowed
            .contains(&"Skill(mattpocock-skills:tdd)".to_string()));
        assert!(
            !p.allowed.iter().any(|a| a == "Bash" || a == "Skill"),
            "never bare Bash or Skill"
        );
        assert!(p.manifest.bash.haruspex_cli.is_none());
        assert!(p.disallowed.contains(&"Bash(git push:*)".to_string()));
    }

    #[test]
    fn reader_challenger_reviewer_and_learner() {
        let none: Vec<String> = vec![];
        let r = policy(Role::Reader, &[], &ctx(&none));
        assert!(r
            .allowed
            .contains(&"Bash(python3 /h/haruspex.py init .)".to_string()));
        assert!(r
            .allowed
            .contains(&"Bash(python3 /h/haruspex.py task activate T-001 .)".to_string()));
        assert!(r
            .manifest
            .write_roots
            .contains(&"/r/.haruspex/tasks/T-001.json".to_string()));
        assert!(r.manifest.write_roots.contains(&"/r/AGENTS.md".to_string()));
        assert!(r.manifest.bash.switch_branch);
        let fresh = Ctx {
            task_id: None,
            initializing: false,
            ..ctx(&none)
        };
        let r = policy(Role::Reader, &[], &fresh);
        assert!(r
            .manifest
            .write_roots
            .contains(&"/r/.haruspex/tasks/".to_string()));
        assert!(!r.manifest.write_roots.contains(&"/r/AGENTS.md".to_string()));
        let c = policy(Role::Challenger, &[], &ctx(&none));
        assert!(!c.manifest.tools.contains(&"Write".to_string()));
        assert_eq!(c.manifest.bash.haruspex, ["gate"]);
        let v = policy(Role::Reviewer, &[], &ctx(&none));
        assert!(v.manifest.bash.prefixes.contains(&"git diff".to_string()));
        assert!(v
            .manifest
            .skills
            .contains(&"mattpocock-skills:code-review".to_string()));
        let l = policy(Role::Learner, &[], &ctx(&none));
        assert!(l.allowed.is_empty() && l.manifest.tools.is_empty());
    }
}
