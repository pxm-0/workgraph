//! The shift parts of `factory.toml` (design §7.1): `[shift]`, `[intake]` and `[[repo]]`. Owner
//! local, validated at load; numbers are clamped to compiled ceilings.

use serde::Deserialize;

pub const MAX_TASKS: u32 = 20;
pub const MAX_PARALLEL_UNITS: u32 = 5;
/// Project agent config a worktree may carry only when the owner allowlisted its hash (§5.1).
pub const PROJECT_CONFIG: [&str; 3] = [
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".mcp.json",
];

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ShiftFile {
    start: Option<String>,
    until: Option<String>,
    max_tasks: Option<u32>,
    max_consecutive_failures: Option<u32>,
    max_parallel_units: Option<u32>,
    attribution: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct IntakeFile {
    label: Option<String>,
    #[serde(default)]
    trusted_actors: Vec<String>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AllowedConfig {
    pub path: String,
    pub sha256: String,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoCfg {
    pub name: String,
    pub path: String,
    #[serde(default = "dev_only")]
    pub pipelines: Vec<String>,
    #[serde(default = "main_branch")]
    pub default_branch: String,
    /// `docker compose -p <name> -f compose.dev.yml -f <owner-local override>`: prefixed to every
    /// check's `factory` string and to `install` in a shift.
    pub factory_compose: Option<String>,
    /// Run once per worktree before the task, e.g. `run --rm -T workspace npm ci`.
    pub install: Option<String>,
    #[serde(default)]
    pub allow_project_config: Vec<AllowedConfig>,
}

fn dev_only() -> Vec<String> {
    vec!["dev".into()]
}

fn main_branch() -> String {
    "main".into()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shift {
    /// Minutes after local midnight.
    pub start: u32,
    pub until: u32,
    pub max_tasks: u32,
    pub max_consecutive_failures: u32,
    pub max_parallel_units: u32,
    pub attribution: String,
    pub label: String,
    pub trusted_actors: Vec<String>,
}

/// `HH:MM` as minutes after midnight.
pub fn minutes(s: &str) -> Result<u32, String> {
    let bad = || format!("{s:?} is not a HH:MM time");
    let (h, m) = s.split_once(':').ok_or_else(bad)?;
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 || s.len() != 5 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

/// `owner/name`, each part `[A-Za-z0-9._-]{1,100}` and not `.` or `..`.
pub fn repo_slug(s: &str) -> Result<(), String> {
    let part = |p: &str| {
        (1..=100).contains(&p.len())
            && p != "."
            && p != ".."
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    };
    match s.split_once('/') {
        Some((o, n)) if part(o) && part(n) => Ok(()),
        _ => Err(format!(
            "{:?} is not an owner/name repo",
            crate::ids::truncate(s, 64)
        )),
    }
}

fn one_line(s: &str, what: &str) -> Result<(), String> {
    if s.trim().is_empty() || s.len() > 512 || s.chars().any(char::is_control) {
        return Err(format!("{what} must be one line of at most 512 bytes"));
    }
    Ok(())
}

fn github_login(s: &str) -> bool {
    (1..=39).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn shift(f: ShiftFile, i: IntakeFile) -> Result<Shift, String> {
    let clamp = |v: Option<u32>, default: u32, max: u32, what: &str| match v {
        Some(0) => Err(format!("[shift].{what} must be positive")),
        Some(n) => Ok(n.min(max)),
        None => Ok(default),
    };
    let attribution = f
        .attribution
        .unwrap_or_else(|| "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>".into());
    one_line(&attribution, "[shift].attribution")?;
    let label = i.label.unwrap_or_else(|| "wg:queued".into());
    crate::ids::gh_label(&label).map_err(|e| format!("[intake].label: {e}"))?;
    if let Some(bad) = i.trusted_actors.iter().find(|a| !github_login(a)) {
        return Err(format!(
            "[intake].trusted_actors: {bad:?} is not a GitHub login"
        ));
    }
    Ok(Shift {
        start: minutes(f.start.as_deref().unwrap_or("01:30"))?,
        until: minutes(f.until.as_deref().unwrap_or("07:00"))?,
        max_tasks: clamp(f.max_tasks, 6, MAX_TASKS, "max_tasks")?,
        max_consecutive_failures: clamp(
            f.max_consecutive_failures,
            3,
            50,
            "max_consecutive_failures",
        )?,
        max_parallel_units: clamp(
            f.max_parallel_units,
            3,
            MAX_PARALLEL_UNITS,
            "max_parallel_units",
        )?,
        attribution,
        label,
        trusted_actors: i.trusted_actors,
    })
}

pub fn repo(r: &RepoCfg) -> Result<(), String> {
    repo_slug(&r.name)?;
    let what = |k: &str| format!("[[repo]] {}: {k}", r.name);
    if !r.path.starts_with('/') || r.path.contains("/../") || r.path.chars().any(char::is_control) {
        return Err(what("path must be absolute"));
    }
    if r.pipelines != ["dev"] {
        return Err(what("Phase 2 runs the pipelines [\"dev\"] only"));
    }
    let b = &r.default_branch;
    if b.is_empty()
        || b.len() > 64
        || b.contains("..")
        || b.starts_with('-')
        || !b
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
    {
        return Err(what("default_branch is not a branch name"));
    }
    if let Some(c) = &r.factory_compose {
        one_line(c, &what("factory_compose"))?;
        if !c.starts_with("docker compose -p ") {
            return Err(what(
                "factory_compose must start with `docker compose -p <name>`",
            ));
        }
    }
    if let Some(i) = &r.install {
        one_line(i, &what("install"))?;
    }
    for a in &r.allow_project_config {
        if !PROJECT_CONFIG.contains(&a.path.as_str()) {
            return Err(what(
                "allow_project_config names a file that is not project agent config",
            ));
        }
        if a.sha256.len() != 64 || !a.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(what("allow_project_config sha256 must be 64 hex digits"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_toml(extra: &str) -> Result<(), String> {
        let r: RepoCfg = toml::from_str(&format!("name = \"o/r\"\npath = \"/src/r\"\n{extra}"))
            .map_err(|e| e.to_string())?;
        repo(&r)
    }

    #[test]
    fn defaults_clamps_and_times() {
        let s = shift(ShiftFile::default(), IntakeFile::default()).unwrap();
        assert_eq!((s.start, s.until, s.max_tasks), (90, 420, 6));
        assert_eq!(s.label, "wg:queued");
        let f: ShiftFile = toml::from_str("max_tasks = 99\nmax_parallel_units = 9").unwrap();
        let s = shift(f, IntakeFile::default()).unwrap();
        assert_eq!(
            (s.max_tasks, s.max_parallel_units),
            (MAX_TASKS, MAX_PARALLEL_UNITS)
        );
        assert_eq!(minutes("23:59"), Ok(1439));
        for bad in ["24:00", "7:00", "07-00", "aa:bb"] {
            assert!(minutes(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn each_rule_rejects_one_bad_value() {
        let f = |t: &str| toml::from_str::<ShiftFile>(t).unwrap();
        let i = |t: &str| toml::from_str::<IntakeFile>(t).unwrap();
        assert!(shift(f("max_tasks = 0"), i(""))
            .unwrap_err()
            .contains("positive"));
        assert!(shift(f("attribution = \"a\\nb\""), i(""))
            .unwrap_err()
            .contains("one line"));
        assert!(shift(f(""), i("label = \"wg:self\""))
            .unwrap_err()
            .contains("owner"));
        assert!(shift(f(""), i("trusted_actors = [\"a b\"]"))
            .unwrap_err()
            .contains("login"));
        assert!(repo_toml("").is_ok());
        for (extra, want) in [
            ("pipelines = [\"dev\", \"qa\"]", "Phase 2"),
            ("default_branch = \"../x\"", "branch name"),
            ("factory_compose = \"docker compose -f x.yml\"", "-p <name>"),
            ("install = \"\"", "one line"),
            (
                "allow_project_config = [{ path = \"x\", sha256 = \"0\" }]",
                "not project agent config",
            ),
            (
                "allow_project_config = [{ path = \".mcp.json\", sha256 = \"0\" }]",
                "64 hex",
            ),
        ] {
            assert!(repo_toml(extra).unwrap_err().contains(want), "{extra}");
        }
        assert!(repo_slug("o/r").is_ok() && repo_slug("o").is_err() && repo_slug("../r").is_err());
        let bad_path: RepoCfg = toml::from_str("name = \"o/r\"\npath = \"rel\"").unwrap();
        assert!(repo(&bad_path).unwrap_err().contains("absolute"));
    }
}
