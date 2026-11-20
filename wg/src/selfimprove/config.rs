//! `[self_improve]` and the self lane's limits in `factory.toml` (design §7.1, §12.1). There are no
//! dollar caps (D3): run counts bound the lane and the eval. Merging is switched on only in
//! `~/.workgraph/self.json`, by `wg self resume --enable-merge`, never here.

use serde::Deserialize;

pub const MAX_EVAL_RUNS_PER_PR: u32 = 5;
pub const MAX_SELF_PRS_PER_DAY: u32 = 3;
pub const MAX_SELF_PRS_PER_WEEK: u32 = 10;
/// Open self PRs at once (design §12.1).
pub const MAX_OPEN_SELF_PRS: usize = 3;
/// Factory proposals filed per shift (design §12.1).
pub const MAX_PROPOSALS_PER_SHIFT: usize = 2;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SelfFile {
    enabled: Option<bool>,
    repo: Option<String>,
    eval_repo: Option<String>,
    /// Where `wg eval capture` pushes; defaults to the eval repo on GitHub. Tests point it at a
    /// local bare repo.
    eval_remote: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelfCfg {
    pub enabled: bool,
    pub repo: String,
    pub eval_repo: String,
    pub eval_remote: String,
    pub max_eval_runs_per_pr: u32,
    pub max_self_prs_per_day: u32,
    pub max_self_prs_per_week: u32,
}

/// The limits that bound the lane, from `[limits]`: each may only lower its compiled ceiling.
pub struct LaneLimits {
    pub eval_runs: Option<u32>,
    pub per_day: Option<u32>,
    pub per_week: Option<u32>,
}

fn remote_ok(r: &str) -> bool {
    let plain = !r.chars().any(|c| c.is_control() || c.is_whitespace());
    plain && (r.starts_with("https://") || (r.starts_with('/') && !r.contains("/../")))
}

pub fn load(f: SelfFile, l: LaneLimits) -> Result<SelfCfg, String> {
    let repo = f.repo.unwrap_or_else(|| "pxm-0/workgraph".into());
    crate::config::repo_slug(&repo).map_err(|e| format!("[self_improve].repo: {e}"))?;
    let eval_repo = f.eval_repo.unwrap_or_else(|| "pxm-0/workgraph-eval".into());
    crate::config::repo_slug(&eval_repo).map_err(|e| format!("[self_improve].eval_repo: {e}"))?;
    if eval_repo == repo {
        return Err("[self_improve].eval_repo must not be the factory's own repo".into());
    }
    let eval_remote = f
        .eval_remote
        .unwrap_or_else(|| format!("https://github.com/{eval_repo}.git"));
    if !remote_ok(&eval_remote) {
        return Err("[self_improve].eval_remote must be an https URL or an absolute path".into());
    }
    let lower = |v: Option<u32>, max: u32, default: u32, what: &str| match v {
        Some(0) => Err(format!("[limits].{what} must be positive")),
        Some(n) => Ok(n.min(max)),
        None => Ok(default),
    };
    Ok(SelfCfg {
        enabled: f.enabled.unwrap_or(false),
        repo,
        eval_repo,
        eval_remote,
        max_eval_runs_per_pr: lower(l.eval_runs, MAX_EVAL_RUNS_PER_PR, 2, "max_eval_runs_per_pr")?,
        max_self_prs_per_day: lower(l.per_day, MAX_SELF_PRS_PER_DAY, 1, "max_self_prs_per_day")?,
        max_self_prs_per_week: lower(
            l.per_week,
            MAX_SELF_PRS_PER_WEEK,
            5,
            "max_self_prs_per_week",
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> LaneLimits {
        LaneLimits {
            eval_runs: None,
            per_day: None,
            per_week: None,
        }
    }

    #[test]
    fn defaults_clamps_and_one_rejection_each() {
        let c = load(SelfFile::default(), none()).unwrap();
        assert!(!c.enabled, "the lane is off until the owner turns it on");
        assert_eq!(c.repo, "pxm-0/workgraph");
        assert_eq!(c.eval_remote, "https://github.com/pxm-0/workgraph-eval.git");
        assert_eq!(
            (
                c.max_eval_runs_per_pr,
                c.max_self_prs_per_day,
                c.max_self_prs_per_week
            ),
            (2, 1, 5)
        );
        let high = LaneLimits {
            eval_runs: Some(99),
            per_day: Some(99),
            per_week: Some(99),
        };
        let c = load(SelfFile::default(), high).unwrap();
        assert_eq!(
            (
                c.max_eval_runs_per_pr,
                c.max_self_prs_per_day,
                c.max_self_prs_per_week
            ),
            (5, 3, 10)
        );
        let f = |t: &str| toml::from_str::<SelfFile>(t).unwrap();
        for (t, want) in [
            ("repo = \"x\"", "repo"),
            ("eval_repo = \"pxm-0/workgraph\"", "must not be"),
            ("eval_remote = \"git@github.com:x/y\"", "https URL"),
            ("eval_remote = \"/a/../b\"", "https URL"),
        ] {
            assert!(load(f(t), none()).unwrap_err().contains(want), "{t}");
        }
        let zero = LaneLimits {
            per_day: Some(0),
            ..none()
        };
        assert!(load(SelfFile::default(), zero)
            .unwrap_err()
            .contains("positive"));
        assert!(
            toml::from_str::<SelfFile>("auto_merge = true").is_err(),
            "merging is switched in self.json only"
        );
    }
}
