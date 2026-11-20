//! Repo-relative paths from untrusted output: relative, normalized, UTF-8, no `..`, not `.git/`.

/// Validates a repo-relative path an agent named and returns it normalized (no `./`, no `//`).
pub fn repo_path(s: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("path {:?} {why}", crate::ids::truncate(s, 80)));
    if s.is_empty() || s.len() > 512 {
        return bad("must be 1-512 bytes");
    }
    if s.starts_with('/') || s.starts_with('~') || s.contains('\\') {
        return bad("must be relative to the repo root");
    }
    if s.chars().any(char::is_control) {
        return bad("has control characters");
    }
    let parts: Vec<&str> = s
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() {
        return bad("names no file");
    }
    if parts.contains(&"..") {
        return bad("must not contain ..");
    }
    if parts[0] == ".git" {
        return bad("must not be under .git/");
    }
    let mut out = parts.join("/");
    if s.ends_with('/') {
        out.push('/');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_paths_are_relative_normalized_and_outside_git() {
        assert_eq!(repo_path("./src//a.js"), Ok("src/a.js".into()));
        assert_eq!(repo_path("docs/"), Ok("docs/".into()));
        for bad in [
            "",
            "/etc/passwd",
            "~/.ssh/id",
            "a/../../b",
            ".git/config",
            "a\\b",
            ".",
            "a\nb",
        ] {
            assert!(repo_path(bad).is_err(), "{bad:?}");
        }
    }
}
