//! The exact `claude -p` argv (design §5.1). Nothing passes through a shell; the prompt goes on
//! stdin. Never used: `--bare`, `bypassPermissions`, `--fallback-model`, `--resume`, `--add-dir`.

use serde_json::Value;
use std::path::Path;
use wg_factory::api::Effort;

pub struct Input<'a> {
    pub model: &'a str,
    pub effort: Option<Effort>,
    pub schema: &'a Value,
    pub max_turns: u32,
    pub runaway_usd: u32,
    pub settings: &'a Path,
    pub allowed: &'a [String],
    pub disallowed: &'a [String],
    pub preamble: &'a str,
}

pub fn argv(i: &Input) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--output-format", "json", "--model", i.model]
        .map(String::from)
        .to_vec();
    if let Some(e) = i.effort {
        a.extend(["--effort".into(), e.name().into()]);
    }
    a.extend([
        "--json-schema".into(),
        i.schema.to_string(),
        "--max-turns".into(),
        i.max_turns.to_string(),
        "--max-budget-usd".into(),
        i.runaway_usd.to_string(),
        "--permission-mode".into(),
        "dontAsk".into(),
        "--setting-sources".into(),
        "user".into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        r#"{"mcpServers":{}}"#.into(),
        "--settings".into(),
        i.settings.to_string_lossy().into_owned(),
    ]);
    if !i.allowed.is_empty() {
        a.push("--allowedTools".into());
        a.extend(i.allowed.iter().cloned());
    }
    if !i.disallowed.is_empty() {
        a.push("--disallowedTools".into());
        a.extend(i.disallowed.iter().cloned());
    }
    a.extend([
        "--append-system-prompt".into(),
        i.preamble.into(),
        "--no-session-persistence".into(),
    ]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_exact_vector() {
        let schema = json!({ "type": "object" });
        let allowed = vec!["Read".to_string(), "Bash(npm test:*)".to_string()];
        let denied = vec!["WebFetch".to_string()];
        let got = argv(&Input {
            model: "sonnet",
            effort: Some(Effort::High),
            schema: &schema,
            max_turns: 30,
            runaway_usd: 25,
            settings: Path::new("/s/challenge.settings.json"),
            allowed: &allowed,
            disallowed: &denied,
            preamble: "POLICY",
        });
        let want = [
            "-p",
            "--output-format",
            "json",
            "--model",
            "sonnet",
            "--effort",
            "high",
            "--json-schema",
            r#"{"type":"object"}"#,
            "--max-turns",
            "30",
            "--max-budget-usd",
            "25",
            "--permission-mode",
            "dontAsk",
            "--setting-sources",
            "user",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--settings",
            "/s/challenge.settings.json",
            "--allowedTools",
            "Read",
            "Bash(npm test:*)",
            "--disallowedTools",
            "WebFetch",
            "--append-system-prompt",
            "POLICY",
            "--no-session-persistence",
        ];
        assert_eq!(got, want);
        let none = argv(&Input {
            effort: None,
            allowed: &[],
            ..Input {
                model: "haiku",
                effort: None,
                schema: &schema,
                max_turns: 3,
                runaway_usd: 25,
                settings: Path::new("/s"),
                allowed: &[],
                disallowed: &denied,
                preamble: "",
            }
        });
        assert!(
            !none.contains(&"--effort".to_string())
                && !none.contains(&"--allowedTools".to_string())
        );
        for banned in [
            "--bare",
            "bypassPermissions",
            "--fallback-model",
            "--resume",
            "--add-dir",
        ] {
            assert!(!got.iter().any(|a| a == banned));
        }
    }
}
