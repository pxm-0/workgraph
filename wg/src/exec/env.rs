//! The environment the kernel's own subprocesses get (checks, Haruspex): the owner's, minus
//! anything that could carry a credential. `claude` children get the stricter allowlist in
//! `claude/auth.rs` instead.

/// True for variables that never reach a subprocess.
pub fn secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|w| n.contains(w))
        || [
            "ANTHROPIC_",
            "CLAUDE_",
            "GH_",
            "GITHUB_",
            "AWS_",
            "OPENAI_",
            "OPENROUTER_",
        ]
        .iter()
        .any(|p| n.starts_with(p))
        || n == "SSH_AUTH_SOCK"
}

/// The current environment without secrets, plus `WG_AGENT` removed so owner tools behave.
pub fn scrubbed() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| !secret(k) && k != "WG_AGENT")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::secret;

    #[test]
    fn credentials_are_dropped_and_ordinary_variables_kept() {
        for s in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_TOKEN",
            "db_password",
        ] {
            assert!(secret(s), "{s}");
        }
        for k in ["PATH", "HOME", "LANG", "DOCKER_HOST", "CARGO_TARGET_DIR"] {
            assert!(!secret(k), "{k}");
        }
    }
}
