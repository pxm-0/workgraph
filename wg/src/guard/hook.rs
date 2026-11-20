//! The PreToolUse decision: path roots, Bash rules, skills and incidents (design §5.4). Pure: it
//! takes the manifest and the hook input and returns a verdict; `cli/guard.rs` does the I/O.

use super::bash::{self, Bash, BashRules};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Written by the kernel per call, outside the worktree, under the agent-denied state dir.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub label: String,
    /// Where `guard.jsonl` goes.
    pub log: String,
    pub worktree: String,
    /// Absolute, canonical directories reads may touch.
    pub read_roots: Vec<String>,
    /// Absolute files, or directories ending in `/`, that Edit and Write may touch.
    pub write_roots: Vec<String>,
    /// Absolute directories whose reads are incidents (`~/.workgraph`, `~/.ssh`, `~/.config`).
    pub secret_roots: Vec<String>,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub bash: BashRules,
    /// What `wg sandbox exec` may do for this call (risky or tainted work, Phase 3).
    #[serde(default)]
    pub sandbox: Option<SandboxGrant>,
}

/// One call's sandbox grant, read by `wg sandbox exec` from the manifest the kernel wrote.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SandboxGrant {
    pub repo: String,
    pub run: String,
    /// A Worker or Fixer's unit, or (for a tainted Reviewer) none.
    pub unit: Option<String>,
    /// A tainted Reviewer's task.
    pub task: Option<String>,
    /// The unit's repo-relative files: the only paths a returned patch may change.
    pub files: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// `effect` is true for an allowed Edit, Write or Bash: the call may have changed something.
    Allow {
        effect: bool,
    },
    Deny(String),
    Incident(String),
}

/// Tools every role may use: structured output and the todo list change nothing outside.
const HARMLESS: [&str; 2] = ["StructuredOutput", "TodoWrite"];
const WRITES: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
const READS: [&str; 4] = ["Read", "Glob", "Grep", "LS"];

/// Canonical form of `p` (absolute, or relative to `cwd`): symlinks and `..` resolved through the
/// deepest existing ancestor, so a path that does not exist yet still resolves.
pub fn canonical(p: &str, cwd: &Path) -> PathBuf {
    let raw = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    let mut tail = Vec::new();
    let mut cur = raw.clone();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for part in tail.iter().rev() {
                match part {
                    s if s == ".." => {
                        out.pop();
                    }
                    s if s == "." => {}
                    s => out.push(s),
                }
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_string_lossy().into_owned());
                cur = parent.to_path_buf();
            }
            _ => {
                // `..` at the end of a missing path has no file_name; keep it and go up.
                if cur.ends_with("..") {
                    tail.push("..".into());
                    cur.pop();
                    continue;
                }
                return raw;
            }
        }
    }
}

fn under_any(path: &Path, roots: &[String]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

/// Writes that are never a mere block (design §5.4).
fn protected_write(rel: &str) -> bool {
    rel == ".haruspex/state.json"
        || [
            ".haruspex/evidence/",
            ".haruspex/bin/",
            ".haruspex/templates/",
            ".haruspex/managed/",
            ".git/",
        ]
        .iter()
        .any(|p| rel.starts_with(p))
        || rel == ".git"
}

fn write_ok(path: &Path, roots: &[String]) -> bool {
    roots.iter().any(|r| match r.strip_suffix('/') {
        Some(dir) => path.starts_with(dir),
        None => path == Path::new(r),
    })
}

fn file_arg(input: &Value) -> Option<&str> {
    input["file_path"]
        .as_str()
        .or_else(|| input["notebook_path"].as_str())
        .or_else(|| input["path"].as_str())
}

pub fn decide(m: &Manifest, hook: &Value) -> Verdict {
    let tool = hook["tool_name"].as_str().unwrap_or("");
    let input = &hook["tool_input"];
    let cwd = PathBuf::from(&m.worktree);
    if HARMLESS.contains(&tool) {
        return Verdict::Allow { effect: false };
    }
    if !m.tools.iter().any(|t| t == tool) {
        return Verdict::Deny(format!("tool {tool:?} is not allowed for this role"));
    }
    if READS.contains(&tool) {
        let target = file_arg(input).unwrap_or(&m.worktree);
        let path = canonical(target, &cwd);
        if under_any(&path, &m.secret_roots) {
            return Verdict::Incident(format!("read of {} is denied", path.display()));
        }
        return if under_any(&path, &m.read_roots) {
            Verdict::Allow { effect: false }
        } else {
            Verdict::Deny(format!("{} is outside the readable roots", path.display()))
        };
    }
    if WRITES.contains(&tool) {
        let Some(target) = file_arg(input) else {
            return Verdict::Deny("a write without a file path".into());
        };
        let path = canonical(target, &cwd);
        let wt = canonical(&m.worktree, Path::new("/"));
        if let Ok(rel) = path.strip_prefix(&wt) {
            if protected_write(&rel.to_string_lossy()) {
                return Verdict::Incident(format!("write to protected {}", rel.display()));
            }
        }
        if under_any(&path, &m.secret_roots) {
            return Verdict::Incident(format!("write to {} is denied", path.display()));
        }
        return if write_ok(&path, &m.write_roots) {
            Verdict::Allow { effect: true }
        } else {
            Verdict::Deny(format!("{} is outside this role's files", path.display()))
        };
    }
    match tool {
        "Bash" => match input["command"].as_str() {
            None => Verdict::Deny("Bash without a command".into()),
            Some(cmd) => match bash::check(&m.bash, cmd) {
                Bash::Allow => Verdict::Allow { effect: true },
                Bash::Deny(why) => Verdict::Deny(why),
                Bash::Incident(why) => Verdict::Incident(why),
            },
        },
        "Skill" => {
            let name = input["skill"].as_str().unwrap_or("");
            if m.skills.iter().any(|s| s == name) {
                Verdict::Allow { effect: false }
            } else {
                Verdict::Deny(format!("skill {name:?} is not allowed for this role"))
            }
        }
        _ => Verdict::Deny(format!("tool {tool:?} is not allowed")),
    }
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod tests;
