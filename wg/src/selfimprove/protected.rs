//! `config/protected.toml` (design §12.2): which paths a self PR may change, the required
//! check-runs, the auto-merge paths and the caps. Everything not listed as editable is protected,
//! including files that do not exist yet. The gate always uses the union of the compiled-in copy
//! and origin/main's, so a PR can never relax the list for itself.

use serde::Deserialize;

/// The copy compiled into this binary (and into base's xtask).
pub const COMPILED: &str = include_str!("../../../config/protected.toml");

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    pub self_editable: Vec<String>,
    pub add_only: Vec<String>,
    pub never_editable: Vec<String>,
    pub required_checks: Vec<(String, String)>,
    pub auto_merge_paths: Vec<String>,
    pub max_changed_lines: u32,
    pub max_changed_files: u32,
    pub min_hours_between_merges: u32,
    pub drift_checkpoint_every: u32,
    pub shadow_prs: u32,
}

pub fn parse(text: &str) -> Result<Rules, String> {
    let r: Rules = toml::from_str(text).map_err(|e| format!("protected.toml: {e}"))?;
    let globs = r
        .self_editable
        .iter()
        .chain(&r.add_only)
        .chain(&r.never_editable)
        .chain(&r.auto_merge_paths);
    if let Some(bad) = globs.into_iter().find(|g| !glob_ok(g)) {
        return Err(format!("protected.toml: {bad:?} is not a repo glob"));
    }
    if r.drift_checkpoint_every == 0 || r.max_changed_files == 0 {
        return Err("protected.toml: caps must be positive".into());
    }
    Ok(r)
}

pub fn compiled() -> Rules {
    parse(COMPILED).expect("the compiled protected.toml is checked by a unit test")
}

fn glob_ok(g: &str) -> bool {
    !g.is_empty()
        && !g.starts_with('/')
        && !g.split('/').any(|s| s.is_empty() || s == "." || s == "..")
}

/// `*` within one path segment, `**` across any number of segments (zero included).
pub fn glob(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    segments(&p, &s)
}

fn segments(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&"**", rest)) => (0..=s.len()).any(|i| segments(rest, &s[i..])),
        Some((first, rest)) => {
            !s.is_empty() && star(first.as_bytes(), s[0].as_bytes()) && segments(rest, &s[1..])
        }
    }
}

fn star(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| star(rest, &s[i..])),
        Some((c, rest)) => s.first() == Some(c) && star(rest, &s[1..]),
    }
}

fn any(globs: &[String], path: &str) -> bool {
    globs.iter().any(|g| glob(g, path))
}

fn intersect(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|g| b.contains(g)).cloned().collect()
}

fn union<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = a.to_vec();
    out.extend(b.iter().filter(|x| !a.contains(x)).cloned());
    out
}

impl Rules {
    /// The stricter of two copies: protections and required checks are united, editable and
    /// auto-merge lists intersected, every cap the tighter one.
    pub fn strictest(&self, other: &Rules) -> Rules {
        Rules {
            self_editable: intersect(&self.self_editable, &other.self_editable),
            add_only: intersect(&self.add_only, &other.add_only),
            never_editable: union(&self.never_editable, &other.never_editable),
            required_checks: union(&self.required_checks, &other.required_checks),
            auto_merge_paths: intersect(&self.auto_merge_paths, &other.auto_merge_paths),
            max_changed_lines: self.max_changed_lines.min(other.max_changed_lines),
            max_changed_files: self.max_changed_files.min(other.max_changed_files),
            min_hours_between_merges: self
                .min_hours_between_merges
                .max(other.min_hours_between_merges),
            drift_checkpoint_every: self
                .drift_checkpoint_every
                .min(other.drift_checkpoint_every),
            shadow_prs: self.shadow_prs.max(other.shadow_prs),
        }
    }

    /// Why `path` may not change with git status `status` (A, M, D, ...), or `None`.
    pub fn refusal(&self, status: char, path: &str) -> Option<String> {
        if crate::paths::repo_path(path).as_deref() != Ok(path) {
            return Some(format!("{path:?} is not a plain repo path"));
        }
        if any(&self.never_editable, path) {
            return Some(format!("{path} is protected"));
        }
        if any(&self.self_editable, path) {
            return None;
        }
        if any(&self.add_only, path) {
            return (status != 'A').then(|| {
                format!("{path} is add-only: an existing file may not change ({status})")
            });
        }
        Some(format!("{path} is not on the editable allowlist"))
    }

    pub fn auto_merges(&self, path: &str) -> bool {
        any(&self.auto_merge_paths, path) && !any(&self.never_editable, path)
    }
}

/// One `git diff --no-renames --name-status -z` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub status: char,
    pub path: String,
}

/// Parses `-z` name-status output. With renames off every entry is one status and one path; a
/// copy or rename entry (two paths) is kept as both sides, each checked.
pub fn parse_name_status(z: &str) -> Result<Vec<Change>, String> {
    let mut out = Vec::new();
    let mut it = z.split('\0').filter(|f| !f.is_empty());
    while let Some(st) = it.next() {
        let status = st
            .chars()
            .next()
            .filter(|c| c.is_ascii_uppercase())
            .ok_or_else(|| format!("git diff: bad status {st:?}"))?;
        let sides = if matches!(status, 'R' | 'C') { 2 } else { 1 };
        for _ in 0..sides {
            let path = it.next().ok_or("git diff: a status without a path")?;
            out.push(Change {
                status,
                path: path.to_string(),
            });
        }
    }
    Ok(out)
}

/// Sums `git diff --numstat -z` lines; a binary change (`-`) is refused outright.
pub fn changed_lines(numstat: &str) -> Result<u64, String> {
    let mut total = 0u64;
    for l in numstat.split('\0').filter(|l| !l.trim().is_empty()) {
        let mut f = l.split('\t');
        let (a, d) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
        if a == "-" || d == "-" {
            return Err(format!(
                "a binary change ({}) cannot be reviewed as text",
                f.next().unwrap_or("?")
            ));
        }
        let n = |s: &str| {
            s.trim()
                .parse::<u64>()
                .map_err(|_| format!("git numstat: {l:?}"))
        };
        total += n(a)? + n(d)?;
    }
    Ok(total)
}

/// What the diff allows (design §12.3 step 2).
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// A protected change or a cap broken: never merged.
    Refused(Vec<String>),
    /// Editable, but outside `auto_merge_paths` (D2): passes, held for the owner.
    Held(Vec<String>),
    AutoMergeable,
}

pub fn verdict(rules: &Rules, changes: &[Change], lines: Result<u64, String>) -> Verdict {
    let mut why: Vec<String> = changes
        .iter()
        .filter_map(|c| rules.refusal(c.status, &c.path))
        .collect();
    let mut files: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    files.dedup();
    if changes.is_empty() {
        why.push("the PR changes nothing".into());
    }
    if files.len() as u64 > u64::from(rules.max_changed_files) {
        why.push(format!(
            "{} files changed, over the cap of {}",
            files.len(),
            rules.max_changed_files
        ));
    }
    match lines {
        Ok(n) if n > u64::from(rules.max_changed_lines) => why.push(format!(
            "{n} lines changed, over the cap of {}",
            rules.max_changed_lines
        )),
        Ok(_) => {}
        Err(e) => why.push(e),
    }
    if !why.is_empty() {
        return Verdict::Refused(why);
    }
    let held: Vec<String> = files
        .iter()
        .filter(|p| !rules.auto_merges(p))
        .map(|p| p.to_string())
        .collect();
    if held.is_empty() {
        Verdict::AutoMergeable
    } else {
        Verdict::Held(held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(status: char, path: &str) -> Change {
        Change {
            status,
            path: path.into(),
        }
    }

    #[test]
    fn globs_match_by_segment() {
        for (g, p, want) in [
            ("wg/**", "wg/tests/flow.rs", true),
            ("wg/**", "wgx/a", false),
            ("prompts/**/*.md", "prompts/work.md", true),
            ("prompts/**/*.md", "prompts/a/b/work.md", true),
            ("prompts/**/*.md", "prompts/work.txt", false),
            ("**/Cargo.toml", "Cargo.toml", true),
            ("**/Cargo.toml", "a/b/Cargo.toml", true),
            ("**/build.rs", "wg-factory/build.rs", true),
            ("docs/**", "docs/design/factory.md", true),
            ("README.md", "README.md", true),
            ("README.md", "docs/README.md", false),
            ("*.md", "a/b.md", false),
        ] {
            assert_eq!(glob(g, p), want, "{g} {p}");
        }
    }

    #[test]
    fn the_compiled_copy_loads_and_each_rule_rejects_one_bad_value() {
        let r = compiled();
        assert_eq!(r.shadow_prs, 5);
        assert_eq!(r.required_checks.len(), 6);
        assert!(parse("x = 1").is_err());
        let bad = COMPILED.replace("\"docs/**\", \"README.md\",\n]", "\"../x\",\n]");
        assert!(parse(&bad).unwrap_err().contains("repo glob"), "{bad}");
        let zero = COMPILED.replace("drift_checkpoint_every = 10", "drift_checkpoint_every = 0");
        assert!(parse(&zero).unwrap_err().contains("positive"));
    }

    #[test]
    fn every_side_of_every_entry_is_checked() {
        let r = compiled();
        assert_eq!(r.refusal('M', "prompts/work.md"), None);
        assert_eq!(r.refusal('A', "wg-factory/src/engine/new.rs"), None);
        assert_eq!(r.refusal('A', "wg-factory/tests/new.rs"), None);
        for (st, p, want) in [
            ('M', "wg/src/gh.rs", "protected"),
            ('M', "prompts/self-review.md", "protected"),
            ('M', "prompts/_preamble.md", "protected"),
            ('A', "wg-factory/build.rs", "protected"),
            ('M', "wg-factory/Cargo.toml", "protected"),
            ('M', "config/protected.toml", "protected"),
            ('D', "eval/replay/happy/case.json", "protected"),
            ('M', "pipelines/self.toml", "protected"),
            ('M', "wg-factory/tests/stages.rs", "add-only"),
            ('D', "wg-factory/tests/stages.rs", "add-only"),
            ('A', "newdir/x.rs", "allowlist"),
            ('A', ".haruspex/state.json", "protected"),
            ('M', "a/../wg/x", "plain repo path"),
        ] {
            let why = r.refusal(st, p).unwrap_or_default();
            assert!(why.contains(want), "{st} {p}: {why}");
        }
    }

    #[test]
    fn a_move_is_a_delete_plus_an_add_and_both_are_checked() {
        let z = "D\0wg/tests/flow.rs\0A\0wg-factory/tests/flow.rs\0M\0docs/a.md\0";
        let ch = parse_name_status(z).unwrap();
        assert_eq!(ch.len(), 3);
        let Verdict::Refused(why) = verdict(&compiled(), &ch, Ok(10)) else {
            panic!("refused")
        };
        assert!(why[0].contains("wg/tests/flow.rs"));
        let renamed = parse_name_status("R100\0docs/a.md\0wg/x.rs\0").unwrap();
        assert_eq!(renamed[1], c('R', "wg/x.rs"));
        assert!(parse_name_status("?\0x\0").is_err());
        assert!(parse_name_status("M\0").is_err());
    }

    #[test]
    fn caps_hold_and_d2_holds_editable_rust_for_the_owner() {
        let r = compiled();
        assert_eq!(
            verdict(&r, &[c('M', "prompts/work.md")], Ok(5)),
            Verdict::AutoMergeable
        );
        assert_eq!(
            verdict(&r, &[c('M', "wg-factory/src/plan/layers.rs")], Ok(5)),
            Verdict::Held(vec!["wg-factory/src/plan/layers.rs".into()])
        );
        let many: Vec<Change> = (0..13).map(|i| c('A', &format!("docs/{i}.md"))).collect();
        assert!(
            matches!(verdict(&r, &many, Ok(13)), Verdict::Refused(w) if w[0].contains("13 files"))
        );
        assert!(
            matches!(verdict(&r, &[c('M', "docs/a.md")], Ok(401)), Verdict::Refused(w) if w[0].contains("401 lines"))
        );
        assert!(matches!(verdict(&r, &[], Ok(0)), Verdict::Refused(w) if w[0].contains("nothing")));
        assert_eq!(changed_lines("3\t4\tdocs/a.md\0").unwrap(), 7);
        assert!(changed_lines("-\t-\tdocs/a.png\0")
            .unwrap_err()
            .contains("binary"));
        assert!(changed_lines("x\t1\ty\0").is_err());
    }

    #[test]
    fn the_strictest_copy_never_relaxes() {
        let base = compiled();
        let relaxed = Rules {
            self_editable: union(&base.self_editable, &["wg/**".to_string()]),
            never_editable: vec![],
            required_checks: vec![],
            max_changed_lines: 10_000,
            shadow_prs: 0,
            ..base.clone()
        };
        let s = base.strictest(&relaxed);
        assert!(s.refusal('M', "wg/src/gh.rs").is_some());
        assert_eq!(s.required_checks, base.required_checks);
        assert_eq!((s.max_changed_lines, s.shadow_prs), (400, 5));
        assert_eq!(relaxed.strictest(&base), s.strictest(&relaxed));
    }
}
