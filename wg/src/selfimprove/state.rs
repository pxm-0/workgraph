//! `~/.workgraph/self.json`: the self lane's switches (design §12.3 step 6, §12.5). Owner local,
//! outside every repo, written atomically. Merging starts off (shadow mode) and is switched on only
//! by `wg self resume --enable-merge` after the shadow decisions; any pause (an owner revert, an
//! unhealthy canary, an incident, `wg self pause`) holds every merge until `wg self resume`.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SelfState {
    pub paused: bool,
    pub pause_reason: Option<String>,
    /// Set only by `wg self resume --enable-merge`; until then the gate runs in shadow mode.
    pub merge_enabled: bool,
    /// Unix seconds of the last resume: the drift checkpoint and owner-revert scan count from here.
    pub resumed_at: u64,
    /// Main SHAs never installed again (a rolled-back binary).
    pub denylist: Vec<String>,
}

pub fn load(path: &Path) -> Result<SelfState, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SelfState::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

pub fn save(path: &Path, s: &SelfState) -> Result<(), String> {
    if let Some(d) = path.parent() {
        crate::state_dirs::private_dir(d)?;
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    crate::shift::record::write_atomic(path, &text)
}

impl SelfState {
    pub fn paused_by(&self, why: &str) -> SelfState {
        SelfState {
            paused: true,
            pause_reason: Some(why.to_string()),
            ..self.clone()
        }
    }

    pub fn denied(&self, sha: &str) -> SelfState {
        let mut denylist = self.denylist.clone();
        if !denylist.iter().any(|d| d == sha) {
            denylist.push(sha.to_string());
        }
        SelfState {
            denylist,
            ..self.clone()
        }
    }

    /// The owner's resume. Enabling merge needs `shadow_done >= shadow_needed` shadow decisions.
    pub fn resumed(
        &self,
        enable_merge: bool,
        shadow_done: u32,
        shadow_needed: u32,
        now: u64,
    ) -> Result<SelfState, String> {
        if enable_merge && shadow_done < shadow_needed {
            return Err(format!(
                "merge stays off: {shadow_done} of {shadow_needed} shadow decisions recorded; let the gate decide in shadow first"
            ));
        }
        Ok(SelfState {
            paused: false,
            pause_reason: None,
            merge_enabled: self.merge_enabled || enable_merge,
            resumed_at: now,
            ..self.clone()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_round_trip_and_merge_needs_the_shadow_decisions() {
        let dir = std::env::temp_dir().join(format!("wg-selfstate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("self.json");
        let s = load(&p).unwrap();
        assert!(!s.merge_enabled && !s.paused, "shadow by default");
        let s = s.paused_by("owner revert").denied("abc").denied("abc");
        save(&p, &s).unwrap();
        let back = load(&p).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.denylist, ["abc"]);
        assert!(back.resumed(true, 4, 5, 9).unwrap_err().contains("4 of 5"));
        let r = back.resumed(true, 5, 5, 9).unwrap();
        assert!(r.merge_enabled && !r.paused && r.resumed_at == 9);
        assert!(back.resumed(false, 0, 5, 9).unwrap().pause_reason.is_none());
        std::fs::write(&p, "{\"paused\": true, \"extra\": 1}").unwrap();
        assert!(load(&p).is_err());
    }
}
