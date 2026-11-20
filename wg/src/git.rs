//! Read-only git queries.

use std::process::Command;

/// Paths from `git status --porcelain=v1 -z`; for renames and copies, the new path.
pub fn dirty() -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .map_err(|e| format!("git status failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_porcelain_z(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_porcelain_z(text: &str) -> Vec<String> {
    let mut parts = text.split('\0').filter(|p| !p.is_empty());
    let mut paths = Vec::new();
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        if entry.starts_with('R') || entry.starts_with('C') {
            parts.next(); // skip the original path
        }
        paths.push(entry[3..].to_string());
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::parse_porcelain_z;

    #[test]
    fn renames_and_copies_report_the_new_path_and_short_entries_are_skipped() {
        let text = "?? b.txt\0R  new.txt\0old.txt\0C  copy.txt\0src.txt\0 M\0 M a.txt\0";
        assert_eq!(
            parse_porcelain_z(text),
            ["b.txt", "new.txt", "copy.txt", "a.txt"]
        );
    }
}
