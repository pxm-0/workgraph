//! `~/.workgraph/factory.toml`: owner-local, validated at load, clamped to compiled limits. The
//! file can only lower a limit, never raise one (design §5.2, §7.1).

use crate::claude::providers::{self, Auth, AuthCfg, ProviderCfg};
use crate::ids;
pub use crate::shift::config::repo_slug;
use crate::shift::config::{IntakeFile, RepoCfg, Shift, ShiftFile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Compiled ceilings.
pub const MAX_CALLS_PER_TASK: u32 = 60;
pub const RUNAWAY_PER_CALL_USD: u32 = 25;
pub const MAX_TURNS: u32 = 100;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    haruspex: Option<Pin>,
    #[serde(default)]
    tools: Tools,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    auth: AuthCfg,
    #[serde(default)]
    providers: BTreeMap<String, ProviderCfg>,
    #[serde(default)]
    claude: ClaudeCfg,
    #[serde(default)]
    check: Vec<CheckEntry>,
    #[serde(default)]
    shift: ShiftFile,
    #[serde(default)]
    intake: IntakeFile,
    #[serde(default)]
    repo: Vec<RepoCfg>,
    #[serde(default)]
    sandbox: crate::sandbox::config::SandboxFile,
}

/// The pinned Haruspex CLI: absolute path and sha256 (design §9.1).
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub cli: String,
    pub sha256: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Tools {
    claude: Option<String>,
    security: Option<String>,
    gh: Option<String>,
    orb: Option<String>,
    orbctl: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Limits {
    max_calls_per_task: Option<u32>,
    runaway_per_call_usd: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ClaudeCfg {
    keep_plugins: Option<Vec<String>>,
}

/// An owner-approved check: what `project.json` holds, and what the kernel runs.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CheckEntry {
    pub repo: String,
    pub name: String,
    pub project: String,
    pub factory: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub haruspex: Option<Pin>,
    pub claude_bin: String,
    pub security_bin: String,
    pub gh_bin: String,
    pub orb_bin: String,
    pub orbctl_bin: String,
    /// The sandbox machine and user (Phase 3).
    pub sandbox: crate::sandbox::config::Machine,
    pub max_calls_per_task: u32,
    pub runaway_per_call_usd: u32,
    pub auth: Auth,
    /// Plugin names (before the `@`) left enabled in children; every other one is turned off.
    pub keep_plugins: Vec<String>,
    pub checks: Vec<CheckEntry>,
    pub shift: Shift,
    pub repos: Vec<RepoCfg>,
}

fn abs_tool(name: &str, v: Option<String>, default: &str) -> Result<String, String> {
    let v = v.unwrap_or_else(|| default.to_string());
    if v.starts_with('/') && !v.chars().any(char::is_control) {
        Ok(v)
    } else {
        Err(format!("[tools].{name} must be an absolute path"))
    }
}

fn one_line_cmd(s: &str, what: &str) -> Result<(), String> {
    if s.trim().is_empty() || s.len() > 512 || s.contains('\n') || s.contains('\r') {
        Err(format!("{what} must be a one-line command"))
    } else {
        Ok(())
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let f: File = toml::from_str(text).map_err(|e| format!("factory.toml: {e}"))?;
    if let Some(p) = &f.haruspex {
        if !p.cli.starts_with('/') {
            return Err("[haruspex].cli must be an absolute path".into());
        }
        if p.sha256.len() != 64 || !p.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("[haruspex].sha256 must be 64 hex digits".into());
        }
    }
    let auth = providers::load(&f.providers, &f.auth)?;
    for c in &f.check {
        ids::check_name(&c.name)?;
        if c.repo.is_empty() || c.repo.len() > 128 || c.repo.contains(char::is_whitespace) {
            return Err(format!("check {}: repo must be a slug", c.name));
        }
        one_line_cmd(&c.project, &format!("check {} project", c.name))?;
        one_line_cmd(&c.factory, &format!("check {} factory", c.name))?;
    }
    for r in &f.repo {
        crate::shift::config::repo(r)?;
    }
    if let Some(dup) = f
        .repo
        .iter()
        .enumerate()
        .find(|(i, r)| f.repo[..*i].iter().any(|x| x.name == r.name))
    {
        return Err(format!("[[repo]] {} is listed twice", dup.1.name));
    }
    let lower = |v: Option<u32>, max: u32, what: &str| match v {
        Some(0) => Err(format!("[limits].{what} must be positive")),
        Some(n) => Ok(n.min(max)),
        None => Ok(max),
    };
    Ok(Config {
        haruspex: f.haruspex,
        claude_bin: abs_tool("claude", f.tools.claude, "/opt/homebrew/bin/claude")?,
        security_bin: abs_tool("security", f.tools.security, "/usr/bin/security")?,
        gh_bin: abs_tool("gh", f.tools.gh, "/opt/homebrew/bin/gh")?,
        orb_bin: abs_tool("orb", f.tools.orb, "/usr/local/bin/orb")?,
        orbctl_bin: abs_tool("orbctl", f.tools.orbctl, "/usr/local/bin/orbctl")?,
        sandbox: crate::sandbox::config::machine(f.sandbox)?,
        max_calls_per_task: lower(
            f.limits.max_calls_per_task,
            MAX_CALLS_PER_TASK,
            "max_calls_per_task",
        )?,
        runaway_per_call_usd: lower(
            f.limits.runaway_per_call_usd,
            RUNAWAY_PER_CALL_USD,
            "runaway_per_call_usd",
        )?,
        auth,
        keep_plugins: f.claude.keep_plugins.unwrap_or_else(|| {
            ["haruspex", "mattpocock-skills", "ecc"]
                .map(String::from)
                .to_vec()
        }),
        checks: f.check,
        shift: crate::shift::config::shift(f.shift, f.intake)?,
        repos: f.repo,
    })
}

/// Loads the file; a missing file means every default.
pub fn load(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => parse(""),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

impl Config {
    pub fn repo(&self, name: &str) -> Option<&RepoCfg> {
        self.repos.iter().find(|r| r.name == name)
    }

    /// The `[[repo]]` a checkout or one of its worktrees belongs to, by its git common dir.
    pub fn repo_at(&self, dir: &Path) -> Option<&RepoCfg> {
        let common = crate::git::git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &[],
        )
        .ok()?;
        let root = Path::new(&common).parent()?.canonicalize().ok()?;
        self.repos
            .iter()
            .find(|r| Path::new(&r.path).canonicalize().is_ok_and(|p| p == root))
    }

    /// The approved entry for a check of `repo`; later entries override earlier ones.
    pub fn check(&self, repo: &str, name: &str) -> Option<&CheckEntry> {
        self.checks
            .iter()
            .rev()
            .find(|c| c.repo == repo && c.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_clamping() {
        let c = parse("").unwrap();
        assert_eq!(c.max_calls_per_task, MAX_CALLS_PER_TASK);
        assert_eq!(c.runaway_per_call_usd, 25);
        assert!(c.haruspex.is_none());
        let c = parse("[limits]\nmax_calls_per_task = 400\nrunaway_per_call_usd = 5\n").unwrap();
        assert_eq!((c.max_calls_per_task, c.runaway_per_call_usd), (60, 5));
    }

    #[test]
    fn later_checks_override_earlier_ones() {
        let text = "[[check]]\nrepo = \"o/r\"\nname = \"test\"\nproject = \"a\"\nfactory = \"a\"\n\
                    [[check]]\nrepo = \"o/r\"\nname = \"test\"\nproject = \"b\"\nfactory = \"b\"\n";
        assert_eq!(
            parse(text).unwrap().check("o/r", "test").unwrap().project,
            "b"
        );
    }

    #[test]
    fn each_rule_rejects_one_bad_file() {
        let sha = "a".repeat(64);
        let cases = [
            ("[nope]\n".to_string(), "unknown field"),
            (
                format!("[haruspex]\ncli = \"rel.py\"\nsha256 = \"{sha}\"\n"),
                "absolute",
            ),
            (
                "[haruspex]\ncli = \"/h.py\"\nsha256 = \"xyz\"\n".into(),
                "64 hex",
            ),
            ("[tools]\nclaude = \"claude\"\n".into(), "absolute path"),
            ("[limits]\nmax_calls_per_task = 0\n".into(), "positive"),
            ("[[repo]]\nname = \"o/r\"\npath = \"/a\"\n[[repo]]\nname = \"o/r\"\npath = \"/b\"\n".into(), "twice"),
            ("[[repo]]\nname = \"o\"\npath = \"/a\"\n".into(), "owner/name"),
            (
                "[[check]]\nrepo = \"o/r\"\nname = \"Bad\"\nproject = \"a\"\nfactory = \"a\"\n"
                    .into(),
                "check name",
            ),
            (
                "[[check]]\nrepo = \"o/r\"\nname = \"t\"\nproject = \"a\\nb\"\nfactory = \"a\"\n"
                    .into(),
                "one-line",
            ),
        ];
        for (text, want) in cases {
            let err = parse(&text).unwrap_err();
            assert!(err.contains(want), "{text} => {err}");
        }
    }
}
